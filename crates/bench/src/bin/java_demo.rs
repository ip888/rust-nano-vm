//! Reproducible production-shape Spring Petclinic demo.
//!
//! This binary intentionally drives `ProcessFleet`, not
//! `KvmHypervisor` directly. Each VM is a real KVM worker behind
//! `nanovm-jailer`, and every HTTP request crosses a lifecycle-owned
//! smoltcp port proxy.

#[cfg(not(feature = "kvm"))]
fn main() {
    eprintln!(
        "nanovm-java-demo: build with --features kvm, then pass the \
         production nanovm-jailer and nanovm-vmm-child binaries"
    );
    std::process::exit(2);
}

#[cfg(any(feature = "kvm", test))]
fn parse_status_line(response: &[u8]) -> Option<u16> {
    let line_end = response.windows(2).position(|w| w == b"\r\n")?;
    let line = std::str::from_utf8(&response[..line_end]).ok()?;
    let status = line.split_ascii_whitespace().nth(1)?;
    status.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::parse_status_line;

    #[test]
    fn parses_http_status_line() {
        assert_eq!(
            parse_status_line(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"),
            Some(200)
        );
        assert_eq!(parse_status_line(b"not http"), None);
    }
}

#[cfg(feature = "kvm")]
mod inner {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, bail, Context, Result};
    use clap::Parser;
    use nanovm_fleet::{FleetConfig, ProcessFleet};
    use vm_core::{
        Hypervisor, NetBackendChoice, PortForward, SnapshotId, VmConfig, VmHandle, VmId,
    };

    use crate::parse_status_line;

    const PETCLINIC_PORT: u16 = 8080;
    const READINESS_PORT: u16 = 8081;
    const READINESS_PATH: &str = "/nanovm-ready";
    const PETCLINIC_PATH: &str = "/vets.html";

    #[derive(Debug, Parser)]
    #[command(
        version,
        about = "Fork a warmed Spring Petclinic VM through the production KVM fleet and verify HTTP"
    )]
    pub(crate) struct Args {
        #[arg(long, env = "NANOVM_TEST_KERNEL")]
        pub kernel: PathBuf,
        #[arg(long, env = "NANOVM_TEST_PETCLINIC_INITRAMFS")]
        pub initramfs: PathBuf,
        #[arg(long, default_value = "/usr/local/bin/nanovm-jailer")]
        pub jailer: PathBuf,
        #[arg(long, default_value = "/usr/local/bin/nanovm-vmm-child")]
        pub vmm_child: PathBuf,
        #[arg(long, default_value = "/tmp/nanovm-java-demo")]
        pub socket_dir: PathBuf,
        #[arg(long)]
        pub snapshot_root: Option<PathBuf>,
        #[arg(long, default_value_t = 2048)]
        pub memory_mib: u64,
        #[arg(long, default_value_t = 3)]
        pub forks: usize,
        #[arg(long, default_value_t = 240)]
        pub warmup_timeout_secs: u64,
        #[arg(long, default_value_t = 20)]
        pub http_timeout_secs: u64,
        #[arg(long)]
        pub cgroup_parent: Option<PathBuf>,
    }

    #[derive(Debug)]
    struct TimedHttp {
        status: u16,
        elapsed: Duration,
        endpoint: SocketAddr,
    }

    #[derive(Debug)]
    struct Sample {
        restore: Duration,
        time_to_ready: Duration,
        http_first_byte: Duration,
    }

    #[derive(Debug, Clone)]
    struct Cancellation {
        cancelled: Arc<AtomicBool>,
        signal: Arc<std::sync::atomic::AtomicI32>,
    }

    impl Cancellation {
        fn install() -> Result<Self> {
            let cancelled = Arc::new(AtomicBool::new(false));
            let signal = Arc::new(std::sync::atomic::AtomicI32::new(0));
            let thread_cancelled = Arc::clone(&cancelled);
            let thread_signal = Arc::clone(&signal);
            std::thread::Builder::new()
                .name("nanovm-demo-signals".into())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("build signal runtime");
                    runtime.block_on(async {
                        #[cfg(unix)]
                        {
                            let mut terminate = tokio::signal::unix::signal(
                                tokio::signal::unix::SignalKind::terminate(),
                            )
                            .expect("install SIGTERM handler");
                            tokio::select! {
                                _ = tokio::signal::ctrl_c() => {
                                    thread_signal.store(2, Ordering::SeqCst);
                                }
                                _ = terminate.recv() => {
                                    thread_signal.store(15, Ordering::SeqCst);
                                }
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            tokio::signal::ctrl_c()
                                .await
                                .expect("install Ctrl-C handler");
                            thread_signal.store(2, Ordering::SeqCst);
                        }
                        thread_cancelled.store(true, Ordering::SeqCst);
                    });
                })
                .context("spawn signal listener")?;
            Ok(Self { cancelled, signal })
        }

        fn check(&self) -> Result<()> {
            if self.cancelled.load(Ordering::SeqCst) {
                bail!(
                    "interrupted by signal {}",
                    self.signal.load(Ordering::SeqCst)
                );
            }
            Ok(())
        }
    }

    struct Cleanup {
        fleet: Arc<ProcessFleet>,
        snapshot: Option<SnapshotId>,
        vms: Vec<VmId>,
    }

    impl Cleanup {
        fn new(fleet: Arc<ProcessFleet>) -> Self {
            Self {
                fleet,
                snapshot: None,
                vms: Vec::new(),
            }
        }

        fn track_vm(&mut self, vm: VmId) {
            self.vms.push(vm);
        }
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(snapshot) = self.snapshot.take() {
                if let Err(error) = self.fleet.delete_snapshot(snapshot) {
                    eprintln!("nanovm-java-demo: snapshot cleanup failed: {error}");
                }
            }
            for vm in self.vms.drain(..).rev() {
                if let Err(error) = self.fleet.destroy(vm) {
                    eprintln!("nanovm-java-demo: VM {vm} cleanup failed: {error}");
                }
            }
        }
    }

    pub(crate) fn run() -> Result<()> {
        let cancellation = Cancellation::install()?;
        let args = Args::parse();
        validate_args(&args)?;
        let kernel = canonicalize_file(&args.kernel, "kernel")?;
        let initramfs = canonicalize_file(&args.initramfs, "initramfs")?;
        let jailer = canonicalize_file(&args.jailer, "jailer")?;
        let vmm_child = canonicalize_file(&args.vmm_child, "vmm-child")?;
        std::fs::create_dir_all(&args.socket_dir)
            .with_context(|| format!("create socket directory {}", args.socket_dir.display()))?;
        let snapshot_root = args
            .snapshot_root
            .clone()
            .unwrap_or_else(|| args.socket_dir.join("snapshots"));
        std::fs::create_dir_all(&snapshot_root)
            .with_context(|| format!("create snapshot root {}", snapshot_root.display()))?;

        let fleet = Arc::new(
            ProcessFleet::new(FleetConfig {
                jailer_binary: jailer,
                vmm_child_binary: vmm_child,
                socket_dir: args.socket_dir.clone(),
                snapshot_root,
                default_memory_limit_mib: None,
                default_cpu_quota_pct: None,
                cgroup_parent: args.cgroup_parent.clone(),
                spawn_timeout: Duration::from_secs(15),
                warm_pool_size: 0,
            })
            .context("start process fleet")?,
        );
        let mut cleanup = Cleanup::new(Arc::clone(&fleet));

        let config = VmConfig {
            vcpus: 1,
            memory_mib: args.memory_mib,
            kernel: Some(kernel),
            initrd: Some(initramfs),
            cmdline: "console=ttyS0,115200 panic=1 rdinit=/init".into(),
            net_backend: NetBackendChoice::Smoltcp,
            port_forwards: vec![
                PortForward {
                    guest_port: PETCLINIC_PORT,
                    host_address: "127.0.0.1".into(),
                    host_port: 0,
                },
                PortForward {
                    guest_port: READINESS_PORT,
                    host_address: "127.0.0.1".into(),
                    host_port: 0,
                },
            ],
            ..VmConfig::default()
        };

        println!("nanovm-java-demo: booting golden Petclinic VM through ProcessFleet");
        let golden_boot = Instant::now();
        cancellation.check()?;
        let golden = fleet.create_vm(&config).context("create golden VM")?;
        cleanup.track_vm(golden.id);
        cancellation.check()?;
        fleet.start(golden.id).context("start golden VM")?;
        let golden_ready = wait_for_http(
            endpoint_for(&golden, READINESS_PORT)?,
            READINESS_PATH,
            Duration::from_secs(args.warmup_timeout_secs),
            &cancellation,
        )
        .context("wait for warmed Petclinic snapshot readiness")?;
        let golden_http = wait_for_http(
            endpoint_for(&golden, PETCLINIC_PORT)?,
            PETCLINIC_PATH,
            Duration::from_secs(args.http_timeout_secs),
            &cancellation,
        )
        .context("exercise Petclinic HTTP endpoint on golden VM")?;
        println!(
            "golden: warmed in {:?}; HTTP {} in {:?} (readiness HTTP first byte {:?}, {})",
            golden_boot.elapsed(),
            golden_http.status,
            golden_http.elapsed,
            golden_ready.elapsed,
            golden_http.endpoint
        );

        let snapshot_started = Instant::now();
        cancellation.check()?;
        let snapshot = fleet
            .snapshot(golden.id)
            .context("snapshot warmed golden VM")?;
        cleanup.snapshot = Some(snapshot);
        println!(
            "snapshot: captured {snapshot} in {:?}",
            snapshot_started.elapsed()
        );

        let mut samples = Vec::with_capacity(args.forks);
        for index in 0..args.forks {
            cancellation.check()?;
            let fork_started = Instant::now();
            let fork = fleet
                .restore(snapshot)
                .with_context(|| format!("restore fork #{}", index + 1))?;
            cleanup.track_vm(fork.id);
            let restore = fork_started.elapsed();
            cancellation.check()?;

            let ready = wait_for_http(
                endpoint_for(&fork, READINESS_PORT)?,
                READINESS_PATH,
                Duration::from_secs(args.http_timeout_secs),
                &cancellation,
            )
            .with_context(|| format!("verify readiness of fork #{}", index + 1))?;
            let time_to_ready = fork_started.elapsed();
            let first_byte = wait_for_http(
                endpoint_for(&fork, PETCLINIC_PORT)?,
                PETCLINIC_PATH,
                Duration::from_secs(args.http_timeout_secs),
                &cancellation,
            )
            .with_context(|| format!("verify Petclinic HTTP on fork #{}", index + 1))?;
            let sample = Sample {
                restore,
                time_to_ready,
                http_first_byte: fork_started.elapsed(),
            };
            println!(
                "fork {:02}: restore={:?} ready={:?} http_first_byte={:?} status={} endpoint={}",
                index + 1,
                sample.restore,
                sample.time_to_ready,
                sample.http_first_byte,
                first_byte.status,
                first_byte.endpoint
            );
            samples.push(sample);
            debug_assert_eq!(ready.status, 200);
        }

        print_report(&samples);
        Ok(())
    }

    fn validate_args(args: &Args) -> Result<()> {
        if args.forks == 0 {
            bail!("--forks must be > 0");
        }
        if args.memory_mib < 1536 {
            bail!("--memory-mib must be at least 1536 for the Java rootfs");
        }
        if args.warmup_timeout_secs == 0 || args.http_timeout_secs == 0 {
            bail!("HTTP timeouts must be > 0");
        }
        Ok(())
    }

    fn canonicalize_file(path: &Path, label: &str) -> Result<PathBuf> {
        path.canonicalize()
            .with_context(|| format!("{label} not found: {}", path.display()))
    }

    fn endpoint_for(handle: &VmHandle, guest_port: u16) -> Result<SocketAddr> {
        let endpoint = handle
            .endpoints
            .iter()
            .find(|endpoint| endpoint.guest_port == guest_port)
            .ok_or_else(|| {
                anyhow!(
                    "VM {} did not return a host endpoint for guest port {guest_port}",
                    handle.id
                )
            })?;
        format!("{}:{}", endpoint.host_address, endpoint.host_port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow!("endpoint resolved to no addresses"))
    }

    fn wait_for_http(
        address: SocketAddr,
        path: &str,
        timeout: Duration,
        cancellation: &Cancellation,
    ) -> Result<TimedHttp> {
        let started = Instant::now();
        let deadline = started + timeout;
        let mut last_error = String::from("server has not accepted a connection");
        loop {
            cancellation.check()?;
            let now = Instant::now();
            if now >= deadline {
                bail!(
                    "GET http://{address}{path} did not return HTTP 200 within {timeout:?}; \
                     last error: {last_error}"
                );
            }
            match http_once(address, path, deadline, cancellation) {
                Ok(status) if status == 200 => {
                    cancellation.check()?;
                    if Instant::now() > deadline {
                        bail!("GET http://{address}{path} completed after its deadline");
                    }
                    return Ok(TimedHttp {
                        status,
                        elapsed: started.elapsed(),
                        endpoint: address,
                    });
                }
                Ok(status) => last_error = format!("HTTP {status}"),
                Err(error) => last_error = error.to_string(),
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn http_once(
        address: SocketAddr,
        path: &str,
        deadline: Instant,
        cancellation: &Cancellation,
    ) -> Result<u16> {
        let remaining = || {
            deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| anyhow!("HTTP deadline expired"))
        };
        cancellation.check()?;
        let connect_timeout = remaining()?.min(Duration::from_millis(250));
        let mut stream = TcpStream::connect_timeout(&address, connect_timeout)
            .with_context(|| format!("connect to {address}"))?;
        let operation_timeout = remaining()?.min(Duration::from_millis(250));
        stream.set_read_timeout(Some(operation_timeout))?;
        stream.set_write_timeout(Some(operation_timeout))?;
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        cancellation.check()?;
        stream.write_all(request.as_bytes())?;

        let mut response = Vec::with_capacity(1024);
        let mut buffer = [0_u8; 256];
        while response.len() < 16 * 1024 {
            cancellation.check()?;
            let read_timeout = remaining()?.min(Duration::from_millis(250));
            stream.set_read_timeout(Some(read_timeout))?;
            let count = match stream.read(&mut buffer) {
                Ok(count) => count,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    if Instant::now() >= deadline {
                        return Err(anyhow!("HTTP deadline expired: {error}"));
                    }
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            cancellation.check()?;
            if count == 0 {
                break;
            }
            response.extend_from_slice(&buffer[..count]);
            if response.windows(2).any(|window| window == b"\r\n") {
                break;
            }
        }
        if Instant::now() > deadline {
            return Err(anyhow!("HTTP deadline expired while reading response"));
        }
        cancellation.check()?;
        parse_status_line(&response).ok_or_else(|| anyhow!("invalid or incomplete HTTP response"))
    }

    fn print_report(samples: &[Sample]) {
        let restore = summary(samples.iter().map(|s| s.restore).collect());
        let ready = summary(samples.iter().map(|s| s.time_to_ready).collect());
        let first_byte = summary(samples.iter().map(|s| s.http_first_byte).collect());
        println!("\n=== production fleet Java demo ===");
        println!("forks verified: {}", samples.len());
        println!("fork/restore p50:      {:?}", restore.0);
        println!("fork/restore p99:      {:?}", restore.1);
        println!("time-to-ready p50:      {:?}", ready.0);
        println!("time-to-ready p99:      {:?}", ready.1);
        println!("HTTP first-byte p50:    {:?}", first_byte.0);
        println!("HTTP first-byte p99:    {:?}", first_byte.1);
    }

    fn summary(mut samples: Vec<Duration>) -> (Duration, Duration) {
        samples.sort_unstable();
        let p50 = percentile(&samples, 0.50);
        let p99 = percentile(&samples, 0.99);
        (p50, p99)
    }

    fn percentile(sorted: &[Duration], percentile: f64) -> Duration {
        if sorted.is_empty() {
            return Duration::ZERO;
        }
        let index = (((sorted.len() - 1) as f64) * percentile).round() as usize;
        sorted[index]
    }

    #[cfg(test)]
    mod tests {
        use super::{http_once, Cancellation};
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        #[test]
        fn http_once_rejects_response_after_absolute_deadline() {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
            let address = listener.local_addr().expect("server address");
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut request = [0; 256];
                let _ = std::io::Read::read(&mut stream, &mut request);
                stream.write_all(b"HTTP/1.1 ").expect("write status prefix");
                std::thread::sleep(Duration::from_millis(150));
                let _ = stream.write_all(b"200 OK\r\nContent-Length: 0\r\n\r\n");
            });

            let cancellation = Cancellation {
                cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                signal: Arc::new(std::sync::atomic::AtomicI32::new(0)),
            };
            let error = http_once(
                address,
                "/",
                Instant::now() + Duration::from_millis(50),
                &cancellation,
            )
            .expect_err("late response must exceed absolute deadline");
            assert!(
                error.to_string().contains("deadline"),
                "unexpected error: {error:#}"
            );
            server.join().expect("server thread");
        }

        #[test]
        fn http_once_continues_reading_through_poll_timeouts() {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
            let address = listener.local_addr().expect("server address");
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut request = [0; 256];
                let _ = std::io::Read::read(&mut stream, &mut request);
                stream
                    .write_all(b"HTTP/1.1 20")
                    .expect("write status prefix");
                std::thread::sleep(Duration::from_millis(350));
                stream
                    .write_all(b"0 OK\r\nContent-Length: 0\r\n\r\n")
                    .expect("finish response");
            });
            let cancellation = Cancellation {
                cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                signal: Arc::new(std::sync::atomic::AtomicI32::new(0)),
            };

            assert_eq!(
                http_once(
                    address,
                    "/",
                    Instant::now() + Duration::from_secs(1),
                    &cancellation,
                )
                .expect("response should survive polling timeouts"),
                200
            );
            server.join().expect("server thread");
        }
    }
}

#[cfg(feature = "kvm")]
fn main() -> anyhow::Result<()> {
    inner::run()
}
