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
        let args = Args::parse();
        validate_args(&args)?;
        let kernel = canonicalize_file(&args.kernel, "kernel")?;
        let initramfs = canonicalize_file(&args.initramfs, "initramfs")?;
        let jailer = canonicalize_file(&args.jailer, "jailer")?;
        let vmm_child = canonicalize_file(&args.vmm_child, "vmm-child")?;
        std::fs::create_dir_all(&args.socket_dir)
            .with_context(|| format!("create socket directory {}", args.socket_dir.display()))?;

        let fleet = Arc::new(
            ProcessFleet::new(FleetConfig {
                jailer_binary: jailer,
                vmm_child_binary: vmm_child,
                socket_dir: args.socket_dir.clone(),
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
        let golden = fleet.create_vm(&config).context("create golden VM")?;
        cleanup.track_vm(golden.id);
        fleet.start(golden.id).context("start golden VM")?;
        let golden_ready = wait_for_http(
            endpoint_for(&golden, READINESS_PORT)?,
            READINESS_PATH,
            Duration::from_secs(args.warmup_timeout_secs),
        )
        .context("wait for warmed Petclinic snapshot readiness")?;
        let golden_http = wait_for_http(
            endpoint_for(&golden, PETCLINIC_PORT)?,
            PETCLINIC_PATH,
            Duration::from_secs(args.http_timeout_secs),
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
            let fork_started = Instant::now();
            let fork = fleet
                .restore(snapshot)
                .with_context(|| format!("restore fork #{}", index + 1))?;
            cleanup.track_vm(fork.id);
            let restore = fork_started.elapsed();

            let ready = wait_for_http(
                endpoint_for(&fork, READINESS_PORT)?,
                READINESS_PATH,
                Duration::from_secs(args.http_timeout_secs),
            )
            .with_context(|| format!("verify readiness of fork #{}", index + 1))?;
            let time_to_ready = fork_started.elapsed();
            let first_byte = wait_for_http(
                endpoint_for(&fork, PETCLINIC_PORT)?,
                PETCLINIC_PATH,
                Duration::from_secs(args.http_timeout_secs),
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

    fn wait_for_http(address: SocketAddr, path: &str, timeout: Duration) -> Result<TimedHttp> {
        let started = Instant::now();
        let deadline = started + timeout;
        let mut last_error = String::from("server has not accepted a connection");
        loop {
            let now = Instant::now();
            if now >= deadline {
                bail!(
                    "GET http://{address}{path} did not return HTTP 200 within {timeout:?}; \
                     last error: {last_error}"
                );
            }
            let connect_timeout = (deadline - now).min(Duration::from_secs(5));
            match http_once(address, path, connect_timeout) {
                Ok(status) if status == 200 => {
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

    fn http_once(address: SocketAddr, path: &str, timeout: Duration) -> Result<u16> {
        let mut stream = TcpStream::connect_timeout(&address, timeout)
            .with_context(|| format!("connect to {address}"))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes())?;

        let mut response = Vec::with_capacity(1024);
        let mut buffer = [0_u8; 256];
        while response.len() < 16 * 1024 {
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            response.extend_from_slice(&buffer[..count]);
            if response.windows(2).any(|window| window == b"\r\n") {
                break;
            }
        }
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
}

#[cfg(feature = "kvm")]
fn main() -> anyhow::Result<()> {
    inner::run()
}
