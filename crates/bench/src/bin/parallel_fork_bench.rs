//! `nanovm-parallel-fork-bench` — the characteristic rust-nano-vm
//! benchmark: **spawn N microVMs *concurrently* from one warm
//! snapshot via `std::thread::spawn`**, measure total wall-clock
//! and the sequential-equivalent speed-up.
//!
//! Positioning:
//!
//! > Firecracker optimises **1→1 restore**. rust-nano-vm optimises
//! > **1→N parallel fork** from the same warm snapshot.
//!
//! The existing [`nanovm-fork-bench`][crate] runs restores
//! sequentially (one destroyed before the next starts) so each
//! sample measures cold-start latency without CPU contention from
//! siblings. That's the right design for latency percentiles, but
//! it under-sells the fork-many pattern — the whole point is that
//! many simultaneous restores share pages via `MAP_PRIVATE`
//! copy-on-write and amortise the per-VM setup across cores.
//!
//! What this binary measures:
//!
//! 1. Boot a golden VM, wait for the agent banner, `hv.snapshot`
//!    it, tear down the golden.
//! 2. Spawn `--forks N` host threads via `std::thread::spawn`.
//!    Each thread independently calls `hv.restore(snap)` and
//!    records its own per-thread wall-clock.
//! 3. `join` every thread. Compute: total wall-clock across all
//!    threads, per-fork min / p50 / p99 / max, host-process RSS
//!    before and after, and the sequential-equivalent speed-up
//!    (`N × mean(per-fork latency) / total wall-clock`).
//! 4. Tear every fork down.
//!
//! The sequential-equivalent is **what Firecracker gets** if a
//! caller does N `CreateSnapshot`/`LoadSnapshot` round-trips back
//! to back; our wall-clock is the comparable number from the
//! parallel path.
//!
//! Build with the `kvm` feature:
//!
//! ```sh
//! cargo run -p bench --release --features kvm --bin nanovm-parallel-fork-bench
//! cargo run -p bench --release --features kvm --bin nanovm-parallel-fork-bench -- \
//!     --forks 50 --verbose
//! ```
//!
//! Without the feature the binary still builds (keeps default
//! workspace checks happy) but refuses to run.

#[cfg(not(feature = "kvm"))]
fn main() {
    eprintln!(
        "nanovm-parallel-fork-bench: build with --features kvm, e.g. \
         `cargo run -p bench --release --features kvm --bin nanovm-parallel-fork-bench`",
    );
    std::process::exit(2);
}

#[cfg(feature = "kvm")]
use std::path::PathBuf;
#[cfg(feature = "kvm")]
use std::sync::Arc;
#[cfg(feature = "kvm")]
use std::time::{Duration, Instant};

#[cfg(feature = "kvm")]
use anyhow::{anyhow, Context, Result};
#[cfg(feature = "kvm")]
use clap::Parser;
#[cfg(feature = "kvm")]
use vm_core::{Hypervisor, SnapshotId, VmConfig, VmId};
#[cfg(feature = "kvm")]
use vm_kvm::KvmHypervisor;

/// Substring the Rust guest-agent prints when it reaches userspace.
/// Matches `AGENT_BANNER` in `vm-kvm/tests/agent_init_boot.rs`.
#[cfg(feature = "kvm")]
const AGENT_BANNER: &str = "nanovm-agent: ready";

#[cfg(feature = "kvm")]
#[derive(Parser, Debug)]
#[command(
    version,
    about = "Measure concurrent fork-many from one snapshot — the rust-nano-vm differentiator"
)]
struct Args {
    /// Number of forks to spawn concurrently in one run.
    #[arg(long, default_value_t = 50)]
    forks: usize,

    /// Repeat the whole boot-golden → snapshot → N-forks → teardown
    /// cycle this many times, and report across-run variance of the
    /// total wall-clock, per-fork mean, and speed-up. Default 1 =
    /// single-run behaviour (unchanged from Week 3 Day 3-4).
    #[arg(long, default_value_t = 1)]
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
        default_value = "tools/initramfs/cache/initramfs-agent.cpio"
    )]
    initrd: PathBuf,

    /// Maximum seconds to wait for the readiness banner before
    /// giving up on golden-VM warm-up.
    #[arg(long, default_value_t = 15)]
    timeout_secs: u64,

    /// Poll interval for the serial-output probe during warm-up,
    /// in milliseconds.
    #[arg(long, default_value_t = 10)]
    poll_interval_ms: u64,

    /// Print per-fork latencies, not just the summary.
    #[arg(long)]
    verbose: bool,
}

/// Boot a golden VM, wait for the banner, and return its handle.
/// Caller is responsible for snapshot + teardown.
#[cfg(feature = "kvm")]
fn boot_golden(hv: &KvmHypervisor, cfg: &VmConfig, args: &Args) -> Result<vm_core::VmHandle> {
    let golden = hv.create_vm(cfg).context("create golden VM")?;
    hv.start(golden.id).context("start golden VM")?;

    let deadline = Instant::now() + Duration::from_secs(args.timeout_secs);
    let poll = Duration::from_millis(args.poll_interval_ms);
    loop {
        let serial = hv
            .serial_output(golden.id)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        if serial.contains(AGENT_BANNER) {
            return Ok(golden);
        }
        if Instant::now() >= deadline {
            let _ = hv.stop(golden.id);
            let _ = hv.destroy(golden.id);
            return Err(anyhow!(
                "golden VM did not reach `{AGENT_BANNER}` within {}s",
                args.timeout_secs
            ));
        }
        std::thread::sleep(poll);
    }
}

/// Spawn `n` threads, each calls `hv.restore(snap)` and records
/// its wall-clock. Returns the sorted per-fork latencies, every
/// restored VM's handle (so the caller tears them down), and the
/// total wall-clock from spawn to last-join.
///
/// Fork handles are returned instead of destroyed-in-thread so the
/// density-side-effect (shared pages present in host RAM) is
/// visible in a post-run RSS sample.
#[cfg(feature = "kvm")]
fn parallel_restore(
    hv: Arc<KvmHypervisor>,
    snap: SnapshotId,
    n: usize,
) -> (Vec<Duration>, Vec<VmId>, Duration) {
    let wall_start = Instant::now();
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let hv_cloned = Arc::clone(&hv);
        let h = std::thread::Builder::new()
            .name(format!("parallel-fork-{i}"))
            .spawn(move || {
                let t = Instant::now();
                let res = hv_cloned.restore(snap);
                let elapsed = t.elapsed();
                res.map(|vm| (vm.id, elapsed))
            })
            .expect("spawn parallel-fork worker thread");
        handles.push(h);
    }

    let mut latencies: Vec<Duration> = Vec::with_capacity(n);
    let mut vm_ids: Vec<VmId> = Vec::with_capacity(n);
    for h in handles {
        match h.join().expect("worker thread panicked") {
            Ok((id, lat)) => {
                vm_ids.push(id);
                latencies.push(lat);
            }
            Err(e) => {
                eprintln!("parallel-fork worker failed: {e:?}");
            }
        }
    }
    let total = wall_start.elapsed();
    latencies.sort();
    (latencies, vm_ids, total)
}

/// `k`-th percentile of `samples`, linear interpolation between
/// the two neighbouring points. `samples` sorted ascending.
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

/// Read our own process RSS from `/proc/self/status` in KiB.
/// `None` if the file isn't readable (shouldn't happen on Linux).
#[cfg(feature = "kvm")]
fn rss_kib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let token = rest.split_whitespace().next()?;
            return token.parse().ok();
        }
    }
    None
}

/// Sample standard deviation (Bessel-corrected) in ms.
#[cfg(feature = "kvm")]
fn stddev_ms(samples: &[f64]) -> f64 {
    let n = samples.len();
    if n < 2 {
        return 0.0;
    }
    let mean = samples.iter().sum::<f64>() / n as f64;
    let var = samples
        .iter()
        .map(|x| {
            let d = x - mean;
            d * d
        })
        .sum::<f64>()
        / (n as f64 - 1.0);
    var.sqrt()
}

/// Headline metrics from one `--forks N` batch.
#[cfg(feature = "kvm")]
#[derive(Debug, Clone)]
struct BatchStats {
    forks_succeeded: usize,
    total_ms: f64,
    per_fork_min_ms: f64,
    per_fork_p50_ms: f64,
    per_fork_mean_ms: f64,
    per_fork_p99_ms: f64,
    per_fork_max_ms: f64,
    speedup: f64,
    rss_delta_kib: Option<i64>,
    rss_per_fork_kib: Option<f64>,
}

/// Do one full batch: boot golden → snapshot → N parallel forks →
/// teardown → delete snapshot. `verbose_latencies` prints the sorted
/// per-fork latencies for this batch (only useful on a single run).
#[cfg(feature = "kvm")]
fn run_one_batch(
    hv: &Arc<KvmHypervisor>,
    cfg: &VmConfig,
    args: &Args,
    verbose_latencies: bool,
) -> Result<BatchStats> {
    // 1. Boot the golden VM.
    let warm_start = Instant::now();
    let golden = boot_golden(hv, cfg, args).context("boot golden")?;
    let warm_ms = warm_start.elapsed().as_secs_f64() * 1000.0;
    eprintln!("  golden warmed in {:>7.1} ms", warm_ms);

    // 2. Snapshot it.
    let snap_start = Instant::now();
    let snap = hv.snapshot(golden.id).context("snapshot golden")?;
    let snap_ms = snap_start.elapsed().as_secs_f64() * 1000.0;
    eprintln!("  snapshot taken ({snap}) in {:>7.1} ms", snap_ms);

    // 3. Golden's vCPU thread is competing with the forks we're
    //    about to spawn — tear it down before the measurement.
    let _ = hv.stop(golden.id);
    let _ = hv.destroy(golden.id);

    let rss_before = rss_kib();

    // 4. Parallel fork-many.
    let (latencies, vm_ids, total) = parallel_restore(Arc::clone(hv), snap, args.forks);
    let total_ms = total.as_secs_f64() * 1000.0;

    if latencies.is_empty() {
        let _ = hv.delete_snapshot(snap);
        return Err(anyhow!("every parallel-fork worker failed; see stderr"));
    }
    if latencies.len() < args.forks {
        eprintln!(
            "  only {}/{} workers succeeded",
            latencies.len(),
            args.forks,
        );
    }

    let rss_after = rss_kib();

    let n = latencies.len();
    let sum_ms: f64 = latencies.iter().map(|d| d.as_secs_f64() * 1000.0).sum();
    let mean_ms = sum_ms / n as f64;
    let min_ms = latencies[0].as_secs_f64() * 1000.0;
    let max_ms = latencies.last().unwrap().as_secs_f64() * 1000.0;
    let p50_ms = percentile_ms(&latencies, 50.0);
    let p99_ms = percentile_ms(&latencies, 99.0);

    if verbose_latencies {
        println!();
        println!("  per-fork latencies (sorted, ms):");
        for (i, d) in latencies.iter().enumerate() {
            println!("    #{:>4}: {:>7.1} ms", i, d.as_secs_f64() * 1000.0);
        }
    }

    let seq_est_ms = mean_ms * n as f64;
    let speedup = if total_ms > 0.0 {
        seq_est_ms / total_ms
    } else {
        0.0
    };

    let (rss_delta_kib, rss_per_fork_kib) = match (rss_before, rss_after) {
        (Some(b), Some(a)) => {
            let delta = a as i64 - b as i64;
            (Some(delta), Some(delta as f64 / n as f64))
        }
        _ => (None, None),
    };

    // 5. Teardown — every restored VM, in parallel. Keep the host
    //    clean even if the user re-runs the bench.
    let teardown_handles: Vec<_> = vm_ids
        .into_iter()
        .map(|id| {
            let hv_cloned = Arc::clone(hv);
            std::thread::spawn(move || {
                let _ = hv_cloned.stop(id);
                let _ = hv_cloned.destroy(id);
            })
        })
        .collect();
    for h in teardown_handles {
        let _ = h.join();
    }
    let _ = hv.delete_snapshot(snap);

    Ok(BatchStats {
        forks_succeeded: n,
        total_ms,
        per_fork_min_ms: min_ms,
        per_fork_p50_ms: p50_ms,
        per_fork_mean_ms: mean_ms,
        per_fork_p99_ms: p99_ms,
        per_fork_max_ms: max_ms,
        speedup,
        rss_delta_kib,
        rss_per_fork_kib,
    })
}

/// Pretty-print the single-batch report (unchanged from pre-`--runs`
/// output, so single-shot callers see the same thing).
#[cfg(feature = "kvm")]
fn print_single_batch(stats: &BatchStats) {
    println!();
    println!(
        "nanovm-parallel-fork-bench: {} parallel forks from one snapshot",
        stats.forks_succeeded
    );
    println!("  total wall-clock       : {:>7.1} ms", stats.total_ms);
    println!();
    println!(
        "  per-fork min           : {:>7.1} ms",
        stats.per_fork_min_ms
    );
    println!(
        "  per-fork p50           : {:>7.1} ms",
        stats.per_fork_p50_ms
    );
    println!(
        "  per-fork mean          : {:>7.1} ms",
        stats.per_fork_mean_ms
    );
    println!(
        "  per-fork p99           : {:>7.1} ms",
        stats.per_fork_p99_ms
    );
    println!(
        "  per-fork max           : {:>7.1} ms",
        stats.per_fork_max_ms
    );

    let seq_est_ms = stats.per_fork_mean_ms * stats.forks_succeeded as f64;
    println!();
    println!(
        "  theoretical sequential : {:>7.1} ms ({} × mean {:.1} ms)",
        seq_est_ms, stats.forks_succeeded, stats.per_fork_mean_ms,
    );
    println!("  parallel wall-clock    : {:>7.1} ms", stats.total_ms);
    println!("  speed-up               : {:>7.1}×", stats.speedup);

    if let (Some(delta), Some(per_fork)) = (stats.rss_delta_kib, stats.rss_per_fork_kib) {
        println!();
        println!(
            "  Δ host RSS             : {:>+7} KiB (per fork {:.1} KiB = {:.2} MiB)",
            delta,
            per_fork,
            per_fork / 1024.0,
        );
    }
}

/// Across-run variance report when `--runs > 1`.
#[cfg(feature = "kvm")]
fn print_aggregate(per_run: &[BatchStats]) {
    let runs = per_run.len();

    // Collect aggregated samples per metric.
    let mut total_ms: Vec<f64> = per_run.iter().map(|s| s.total_ms).collect();
    let mut p50_ms: Vec<f64> = per_run.iter().map(|s| s.per_fork_p50_ms).collect();
    let mut p99_ms: Vec<f64> = per_run.iter().map(|s| s.per_fork_p99_ms).collect();
    let mut speedup: Vec<f64> = per_run.iter().map(|s| s.speedup).collect();

    total_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    p50_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    p99_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    speedup.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let idx = |v: &[f64], pct: f64| -> f64 {
        let n = v.len();
        if n == 1 {
            return v[0];
        }
        let f = (pct / 100.0) * (n as f64 - 1.0);
        let lo = f.floor() as usize;
        let hi = f.ceil() as usize;
        v[lo] + (v[hi] - v[lo]) * (f - lo as f64)
    };
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;

    println!();
    println!("────────────────────────────────────────────────────────");
    println!("  Across-run variance ({} runs)", runs);
    println!("────────────────────────────────────────────────────────");
    println!(
        "{:<24} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "metric", "min", "p50", "mean", "p99", "stddev",
    );

    let row = |label: &str, v: &[f64], unit: &str| {
        println!(
            "{:<24} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1}  {}",
            label,
            v[0],
            idx(v, 50.0),
            mean(v),
            idx(v, 99.0),
            stddev_ms(v),
            unit,
        );
    };

    row("total wall-clock", &total_ms, "ms");
    row("per-fork p50", &p50_ms, "ms");
    row("per-fork p99", &p99_ms, "ms");
    println!(
        "{:<24} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1}  ×",
        "speed-up",
        speedup[0],
        idx(&speedup, 50.0),
        mean(&speedup),
        idx(&speedup, 99.0),
        stddev_ms(&speedup),
    );

    // Memory per-fork — show mean across runs if available.
    let rss_samples: Vec<f64> = per_run.iter().filter_map(|s| s.rss_per_fork_kib).collect();
    if !rss_samples.is_empty() {
        println!();
        println!(
            "  memory per fork (mean across runs) : {:>7.1} KiB = {:.2} MiB",
            mean(&rss_samples),
            mean(&rss_samples) / 1024.0,
        );
    }
}

#[cfg(feature = "kvm")]
fn main() -> Result<()> {
    let args = Args::parse();
    if args.forks == 0 {
        return Err(anyhow!("--forks must be > 0"));
    }
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
        "nanovm-parallel-fork-bench: kernel={} initrd={}",
        args.kernel.display(),
        args.initrd.display(),
    );
    println!(
        "nanovm-parallel-fork-bench: forks={} runs={} memory={} MiB",
        args.forks, args.runs, args.memory_mib,
    );

    // `Arc<KvmHypervisor>` so we can hand clones to spawned worker
    // threads without widening the trait.
    let hv = Arc::new(KvmHypervisor::new().context("open /dev/kvm")?);
    let cfg = VmConfig {
        vcpus: 1,
        memory_mib: args.memory_mib,
        kernel: Some(args.kernel.clone()),
        initrd: Some(args.initrd.clone()),
        cmdline: "console=ttyS0,115200 panic=-1 rdinit=/init".into(),
        ..VmConfig::default()
    };

    let mut per_run: Vec<BatchStats> = Vec::with_capacity(args.runs);
    for run_idx in 0..args.runs {
        if args.runs > 1 {
            println!();
            println!("=== run {}/{} ===", run_idx + 1, args.runs);
        }
        let stats = run_one_batch(&hv, &cfg, &args, args.verbose && args.runs == 1)
            .with_context(|| format!("parallel-fork run {}/{}", run_idx + 1, args.runs))?;

        if args.runs == 1 {
            // Preserve the original single-shot output exactly.
            print_single_batch(&stats);
        } else {
            // Compact per-run summary line when running many.
            println!(
                "  → wall={:>7.1} ms  p50={:>5.1} ms  p99={:>5.1} ms  speedup={:>5.1}×",
                stats.total_ms, stats.per_fork_p50_ms, stats.per_fork_p99_ms, stats.speedup,
            );
        }
        per_run.push(stats);
    }

    if args.runs > 1 {
        print_aggregate(&per_run);
    }

    Ok(())
}
