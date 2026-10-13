//! `nanovm-boot-bench` — measures **cold boot** latency of a bare
//! Linux microVM, from `create_vm()` to the moment the guest's init
//! writes its readiness banner to the serial console.
//!
//! Sister binary to [`nanovm-fork-bench`][crate]. Where fork-bench
//! times the `snapshot → restore` data-plane (what Firecracker also
//! optimises), **boot-bench times the first-time cold path** —
//! kernel decompression + early userspace. That's the baseline a
//! reader needs before they can appreciate how much faster the
//! snapshot/restore and fork-many flows are.
//!
//! Each run:
//!   1. `create_vm` the hypervisor (clock starts here).
//!   2. `start()` the VM.
//!   3. Poll `serial_output()` for the agent's `"nanovm-agent:
//!      ready"` banner (any userspace-reachable signal would do;
//!      the agent banner is the earliest one the existing fixture
//!      produces).
//!   4. Stop the clock, tear down the VM.
//!
//! N consecutive runs → min / p50 / p99 / max / mean latency.
//!
//! Build with the `kvm` feature (same deal as `nanovm-fork-bench`):
//!
//! ```sh
//! cargo run -p bench --release --features kvm --bin nanovm-boot-bench
//! ```
//!
//! Without the feature the binary still builds (keeps default
//! workspace checks happy) but refuses to run.

#[cfg(not(feature = "kvm"))]
fn main() {
    eprintln!(
        "nanovm-boot-bench: build with --features kvm, e.g. \
         `cargo run -p bench --release --features kvm --bin nanovm-boot-bench`",
    );
    std::process::exit(2);
}

#[cfg(feature = "kvm")]
use std::path::PathBuf;
#[cfg(feature = "kvm")]
use std::time::{Duration, Instant};

#[cfg(feature = "kvm")]
use anyhow::{anyhow, Context, Result};
#[cfg(feature = "kvm")]
use clap::Parser;
#[cfg(feature = "kvm")]
use vm_core::{Hypervisor, VmConfig};
#[cfg(feature = "kvm")]
use vm_kvm::KvmHypervisor;

/// Substring the Rust guest-agent prints when it reaches userspace.
/// Matches `AGENT_BANNER` in `vm-kvm/tests/agent_init_boot.rs`.
#[cfg(feature = "kvm")]
const AGENT_BANNER: &str = "nanovm-agent: ready";

#[cfg(feature = "kvm")]
#[derive(Parser, Debug)]
#[command(version, about = "Measure nanovm cold-boot latency")]
struct Args {
    /// Number of cold-boot runs to perform.
    #[arg(long, default_value_t = 10)]
    runs: usize,

    /// Guest memory in MiB.
    #[arg(long, default_value_t = 128)]
    memory_mib: u64,

    /// Guest kernel image (bzImage).
    #[arg(
        long,
        env = "NANOVM_TEST_KERNEL",
        default_value = "tools/kernel/cache/bzImage"
    )]
    kernel: PathBuf,

    /// Guest initramfs (cpio).
    #[arg(
        long,
        env = "NANOVM_TEST_INITRAMFS",
        default_value = "tools/initramfs/cache/initramfs.cpio"
    )]
    initrd: PathBuf,

    /// Maximum seconds to wait for the readiness banner before
    /// giving up on an individual run.
    #[arg(long, default_value_t = 15)]
    timeout_secs: u64,

    /// Poll interval for the serial-output probe, in milliseconds.
    /// Shorter → more accurate latency, more CPU; 10 ms is enough
    /// to not dominate the measurement noise for sub-second boots.
    #[arg(long, default_value_t = 10)]
    poll_interval_ms: u64,

    /// Print the elapsed time for every run (not just the summary).
    #[arg(long)]
    verbose: bool,
}

/// One run of `create_vm → start → banner`. Returns the elapsed
/// wall-clock on success; cleans up the VM on both paths.
#[cfg(feature = "kvm")]
fn one_boot(hv: &KvmHypervisor, cfg: &VmConfig, args: &Args) -> Result<Duration> {
    let start = Instant::now();
    let handle = hv.create_vm(cfg).context("create_vm")?;
    hv.start(handle.id).context("start VM")?;

    let deadline = start + Duration::from_secs(args.timeout_secs);
    let poll = Duration::from_millis(args.poll_interval_ms);
    let elapsed_or_timeout = loop {
        let serial = hv
            .serial_output(handle.id)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        if serial.contains(AGENT_BANNER) {
            break Ok(start.elapsed());
        }
        if Instant::now() >= deadline {
            break Err(anyhow!(
                "VM did not reach `{AGENT_BANNER}` within {}s\n  serial:\n{serial}",
                args.timeout_secs
            ));
        }
        std::thread::sleep(poll);
    };

    // Teardown — do it on both the success and the error path so a
    // stuck VM doesn't poison the next run.
    let _ = hv.stop(handle.id);
    let _ = hv.destroy(handle.id);
    elapsed_or_timeout
}

/// `k`-th percentile of `samples`, with linear interpolation
/// between the two neighbouring points (same formula pandas /
/// numpy use by default). `samples` is already sorted ascending.
#[cfg(feature = "kvm")]
fn percentile_ms(samples: &[Duration], k: f64) -> f64 {
    debug_assert!(!samples.is_empty());
    let n = samples.len();
    if n == 1 {
        return samples[0].as_secs_f64() * 1000.0;
    }
    let idx_f = (k / 100.0) * (n as f64 - 1.0);
    let lo = idx_f.floor() as usize;
    let hi = idx_f.ceil() as usize;
    let frac = idx_f - lo as f64;
    let lo_ms = samples[lo].as_secs_f64() * 1000.0;
    let hi_ms = samples[hi].as_secs_f64() * 1000.0;
    lo_ms + (hi_ms - lo_ms) * frac
}

#[cfg(feature = "kvm")]
fn main() -> Result<()> {
    let args = Args::parse();
    if args.runs == 0 {
        return Err(anyhow!("--runs must be > 0"));
    }
    if !args.kernel.exists() {
        return Err(anyhow!("kernel not found: {}", args.kernel.display()));
    }
    if !args.initrd.exists() {
        return Err(anyhow!("initrd not found: {}", args.initrd.display()));
    }

    println!(
        "nanovm-boot-bench: kernel={} initrd={}",
        args.kernel.display(),
        args.initrd.display(),
    );
    println!(
        "nanovm-boot-bench: runs={} memory={} MiB timeout={}s",
        args.runs, args.memory_mib, args.timeout_secs,
    );

    let hv = KvmHypervisor::new().context("open /dev/kvm")?;
    let cfg = VmConfig {
        vcpus: 1,
        memory_mib: args.memory_mib,
        kernel: Some(args.kernel.clone()),
        initrd: Some(args.initrd.clone()),
        cmdline: "console=ttyS0,115200 panic=-1 rdinit=/init".into(),
        ..VmConfig::default()
    };

    let mut latencies: Vec<Duration> = Vec::with_capacity(args.runs);
    for i in 0..args.runs {
        let d = one_boot(&hv, &cfg, &args)
            .with_context(|| format!("run #{} of {}", i + 1, args.runs))?;
        if args.verbose {
            println!(
                "nanovm-boot-bench: run {:>3} / {} → {:>7.1} ms",
                i + 1,
                args.runs,
                d.as_secs_f64() * 1000.0
            );
        }
        latencies.push(d);
    }

    latencies.sort();
    let sum_ms: f64 = latencies.iter().map(|d| d.as_secs_f64() * 1000.0).sum();
    let mean_ms = sum_ms / latencies.len() as f64;
    let min_ms = latencies[0].as_secs_f64() * 1000.0;
    let max_ms = latencies.last().unwrap().as_secs_f64() * 1000.0;
    let p50_ms = percentile_ms(&latencies, 50.0);
    let p99_ms = percentile_ms(&latencies, 99.0);

    println!();
    println!("nanovm-boot-bench: cold-boot latency ({} runs)", args.runs);
    println!("  min   : {:>7.1} ms", min_ms);
    println!("  p50   : {:>7.1} ms", p50_ms);
    println!("  mean  : {:>7.1} ms", mean_ms);
    println!("  p99   : {:>7.1} ms", p99_ms);
    println!("  max   : {:>7.1} ms", max_ms);
    Ok(())
}
