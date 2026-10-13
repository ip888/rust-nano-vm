//! `nanovm-boot-bench` — measures microVM cold-start latency in two
//! modes side by side: **cold boot** (kernel decompression + early
//! userspace, no snapshot in the loop) and **restore** (resuming a
//! pre-captured snapshot via the in-process `hv.restore(snap)` API).
//!
//! Sister binary to [`nanovm-fork-bench`][crate]. Where fork-bench
//! stress-tests the "snapshot once, fork many" data plane with
//! density + warm-pool phases, **boot-bench produces the single
//! headline number** readers compare against Firecracker: cold boot
//! vs snapshot restore, both in ms, both from the same workload
//! setup so the ratio is honest.
//!
//! Mode | What the clock measures
//! -----|------------------------
//! `cold`    | `create_vm → start → serial banner "nanovm-agent: ready"`
//! `restore` | `hv.restore(snap)` wall-clock (returns a running VM)
//! `both`    | Cold phase, then restore phase. Prints both tables.
//!
//! N consecutive runs → min / p50 / p99 / max / mean latency per
//! mode. The restore phase prints the cold-vs-restore speed-up
//! ratio when `--mode both` is passed.
//!
//! Build with the `kvm` feature (same deal as `nanovm-fork-bench`):
//!
//! ```sh
//! cargo run -p bench --release --features kvm --bin nanovm-boot-bench
//! cargo run -p bench --release --features kvm --bin nanovm-boot-bench -- \
//!     --mode both --runs 20 --verbose
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
use clap::{Parser, ValueEnum};
#[cfg(feature = "kvm")]
use vm_core::{Hypervisor, SnapshotId, VmConfig};
#[cfg(feature = "kvm")]
use vm_kvm::KvmHypervisor;

/// Substring the Rust guest-agent prints when it reaches userspace.
/// Matches `AGENT_BANNER` in `vm-kvm/tests/agent_init_boot.rs`.
#[cfg(feature = "kvm")]
const AGENT_BANNER: &str = "nanovm-agent: ready";

/// What to measure. See the module docs for what the clock covers
/// in each mode.
#[cfg(feature = "kvm")]
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Cold boot latency only — no snapshot in the loop.
    Cold,
    /// Restore-from-snapshot latency only. One warm boot takes a
    /// snapshot up front, then N restores from that same snapshot.
    Restore,
    /// Both modes in sequence; prints a cold-vs-restore speed-up.
    Both,
}

#[cfg(feature = "kvm")]
#[derive(Parser, Debug)]
#[command(version, about = "Measure nanovm cold-boot and restore latency")]
struct Args {
    /// Which measurement to run.
    #[arg(long, value_enum, default_value_t = Mode::Cold)]
    mode: Mode,

    /// Number of runs per measurement (both cold and restore use
    /// the same `--runs` so the summaries are directly comparable).
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

/// One `hv.restore(snap)` round-trip — API-level restore latency.
/// Returns the elapsed wall-clock on success; destroys the restored
/// VM on both paths so the next iteration measures a fresh restore.
///
/// Measures the host-side API, not "time to first byte" of guest
/// work. That matches what a serverless-style caller observes
/// ("I got a running VM back"); workload-specific readiness
/// numbers (first HTTP request answered, etc.) depend on what the
/// snapshot captured and are out of scope for this harness.
#[cfg(feature = "kvm")]
fn one_restore(hv: &KvmHypervisor, snap: SnapshotId) -> Result<Duration> {
    let start = Instant::now();
    let fork = hv.restore(snap).context("restore from snapshot")?;
    let elapsed = start.elapsed();
    // Teardown — keep each iteration independent.
    let _ = hv.stop(fork.id);
    let _ = hv.destroy(fork.id);
    Ok(elapsed)
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

/// Summary of one measurement phase. Carries both the raw stats and
/// the sample count so a caller doing a cold-vs-restore comparison
/// can print a speed-up ratio.
#[cfg(feature = "kvm")]
struct PhaseStats {
    label: &'static str,
    runs: usize,
    min_ms: f64,
    p50_ms: f64,
    mean_ms: f64,
    p99_ms: f64,
    max_ms: f64,
}

#[cfg(feature = "kvm")]
impl PhaseStats {
    fn from_sorted(label: &'static str, latencies: &[Duration]) -> Self {
        let runs = latencies.len();
        debug_assert!(runs > 0);
        let sum_ms: f64 = latencies.iter().map(|d| d.as_secs_f64() * 1000.0).sum();
        Self {
            label,
            runs,
            min_ms: latencies[0].as_secs_f64() * 1000.0,
            p50_ms: percentile_ms(latencies, 50.0),
            mean_ms: sum_ms / runs as f64,
            p99_ms: percentile_ms(latencies, 99.0),
            max_ms: latencies.last().unwrap().as_secs_f64() * 1000.0,
        }
    }

    fn print(&self) {
        println!();
        println!(
            "nanovm-boot-bench: {} latency ({} runs)",
            self.label, self.runs,
        );
        println!("  min   : {:>7.1} ms", self.min_ms);
        println!("  p50   : {:>7.1} ms", self.p50_ms);
        println!("  mean  : {:>7.1} ms", self.mean_ms);
        println!("  p99   : {:>7.1} ms", self.p99_ms);
        println!("  max   : {:>7.1} ms", self.max_ms);
    }
}

/// Run the cold-boot phase: N independent `create_vm → start →
/// banner → destroy` cycles. Returns sorted latencies.
#[cfg(feature = "kvm")]
fn run_cold_phase(hv: &KvmHypervisor, cfg: &VmConfig, args: &Args) -> Result<Vec<Duration>> {
    let mut latencies: Vec<Duration> = Vec::with_capacity(args.runs);
    for i in 0..args.runs {
        let d = one_boot(hv, cfg, args)
            .with_context(|| format!("cold run #{} of {}", i + 1, args.runs))?;
        if args.verbose {
            println!(
                "nanovm-boot-bench: cold run {:>3} / {} → {:>7.1} ms",
                i + 1,
                args.runs,
                d.as_secs_f64() * 1000.0,
            );
        }
        latencies.push(d);
    }
    latencies.sort();
    Ok(latencies)
}

/// Run the restore phase: one warm boot to the banner, snapshot
/// that golden VM, then N independent restores from the same
/// snapshot. Returns sorted latencies.
///
/// We tear the golden VM down after snapshotting so its vCPU
/// thread isn't competing with the restored VMs for host CPU.
#[cfg(feature = "kvm")]
fn run_restore_phase(hv: &KvmHypervisor, cfg: &VmConfig, args: &Args) -> Result<Vec<Duration>> {
    // 1. Boot a golden VM and wait for the banner so the snapshot
    //    captures a VM that has already finished early userspace.
    let warm_start = Instant::now();
    let one_boot_elapsed = one_boot(hv, cfg, args).context("warm up golden VM for snapshot")?;
    // `one_boot` destroyed the VM — we need it alive to snapshot.
    // Re-create it instead so the lifecycle is "create → start →
    // banner → snapshot → destroy".
    let golden = hv.create_vm(cfg).context("re-create golden for snapshot")?;
    hv.start(golden.id).context("start golden for snapshot")?;
    let deadline = Instant::now() + Duration::from_secs(args.timeout_secs);
    let poll = Duration::from_millis(args.poll_interval_ms);
    loop {
        let serial = hv
            .serial_output(golden.id)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        if serial.contains(AGENT_BANNER) {
            break;
        }
        if Instant::now() >= deadline {
            let _ = hv.stop(golden.id);
            let _ = hv.destroy(golden.id);
            return Err(anyhow!(
                "golden VM did not reach `{AGENT_BANNER}` within {}s during restore-phase warmup",
                args.timeout_secs
            ));
        }
        std::thread::sleep(poll);
    }
    if args.verbose {
        println!(
            "nanovm-boot-bench: warm-up boot took {:>7.1} ms, re-warm {:>7.1} ms",
            one_boot_elapsed.as_secs_f64() * 1000.0,
            warm_start.elapsed().as_secs_f64() * 1000.0,
        );
    }

    // 2. Snapshot the golden. The timer isn't measuring snapshot
    //    latency itself (not what this binary advertises); we
    //    print it for context so a reader knows the amortised
    //    cost of entering the restore regime.
    let snap_start = Instant::now();
    let snap = hv.snapshot(golden.id).context("snapshot golden VM")?;
    let snap_ms = snap_start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "nanovm-boot-bench: snapshot taken ({snap}, {:>7.1} ms)",
        snap_ms
    );
    // Golden served its purpose — free the vCPU thread before
    // the restore-benchmark loop so it's not competing for CPU.
    let _ = hv.stop(golden.id);
    let _ = hv.destroy(golden.id);

    // 3. N restores from the same snapshot. Each is destroyed
    //    before the next starts, same pattern as the cold phase
    //    so the two measurements are directly comparable.
    let mut latencies: Vec<Duration> = Vec::with_capacity(args.runs);
    for i in 0..args.runs {
        let d = one_restore(hv, snap)
            .with_context(|| format!("restore run #{} of {}", i + 1, args.runs))?;
        if args.verbose {
            println!(
                "nanovm-boot-bench: restore run {:>3} / {} → {:>7.1} ms",
                i + 1,
                args.runs,
                d.as_secs_f64() * 1000.0,
            );
        }
        latencies.push(d);
    }
    latencies.sort();
    Ok(latencies)
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
        "nanovm-boot-bench: mode={:?} runs={} memory={} MiB timeout={}s",
        args.mode, args.runs, args.memory_mib, args.timeout_secs,
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

    let mut cold: Option<PhaseStats> = None;
    let mut restore: Option<PhaseStats> = None;

    if matches!(args.mode, Mode::Cold | Mode::Both) {
        let latencies = run_cold_phase(&hv, &cfg, &args)?;
        let stats = PhaseStats::from_sorted("cold boot", &latencies);
        stats.print();
        cold = Some(stats);
    }

    if matches!(args.mode, Mode::Restore | Mode::Both) {
        let latencies = run_restore_phase(&hv, &cfg, &args)?;
        let stats = PhaseStats::from_sorted("restore", &latencies);
        stats.print();
        restore = Some(stats);
    }

    // When both modes ran, print the headline speed-up: cold.p50 /
    // restore.p50. That ratio is what a reader skims for in the
    // README table.
    if let (Some(c), Some(r)) = (cold.as_ref(), restore.as_ref()) {
        println!();
        println!("nanovm-boot-bench: cold vs restore (p50)");
        println!("  cold.p50    : {:>7.1} ms", c.p50_ms);
        println!("  restore.p50 : {:>7.1} ms", r.p50_ms);
        if r.p50_ms > 0.0 {
            println!("  speed-up    : {:>7.1}×", c.p50_ms / r.p50_ms);
        }
    }

    Ok(())
}
