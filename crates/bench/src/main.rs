//! `nanovm-fork-bench` — measures the cold-start latency of the
//! snapshot → fork data plane on the host.
//!
//! Boots a "golden" agent VM (tick mode, so it stays runnable and
//! observably alive), snapshots it once, then forks N times sequentially
//! (each fork destroyed before the next is created — keeps CPU clean so
//! the latency numbers are honest). Per-fork wall time is `restore()` →
//! the returned VM handle. After the run we print min / P50 / P95 / P99 /
//! max / mean fork latency, total wall time, throughput, and the host
//! process RSS before/after the run.
//!
//! This is the "what to put in front of a customer" artifact for the
//! snapshot-once-fork-many moat. Build with `--features kvm`:
//!
//!     cargo run -p bench --release --features kvm --bin nanovm-fork-bench
//!
//! Without the feature the binary builds (so it lands in default workspace
//! checks) but refuses to run.

#[cfg(not(feature = "kvm"))]
fn main() {
    eprintln!(
        "nanovm-fork-bench: build with --features kvm, e.g. \
         `cargo run -p bench --release --features kvm --bin nanovm-fork-bench`",
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
use vm_core::{Hypervisor, VmConfig, VmId};
#[cfg(feature = "kvm")]
use vm_kvm::KvmHypervisor;

#[cfg(feature = "kvm")]
#[derive(Parser, Debug)]
#[command(version, about = "Measure nanovm fork latency and host RSS")]
struct Args {
    /// Number of forks to perform.
    #[arg(long, default_value_t = 100)]
    forks: usize,

    /// Guest memory size (MiB).
    #[arg(long, default_value_t = 128)]
    memory_mib: u64,

    /// Path to a bzImage Linux kernel.
    #[arg(long, env = "NANOVM_TEST_KERNEL")]
    kernel: PathBuf,

    /// Path to the agent initramfs (the host appends `NANOVM_AGENT_TICK=1`
    /// to the cmdline, so this should be the `agent` variant).
    #[arg(long, env = "NANOVM_TEST_AGENT_INITRAMFS")]
    initrd: PathBuf,

    /// Seconds to wait for the golden VM to reach tick mode before giving up.
    #[arg(long, default_value_t = 40)]
    warm_secs: u64,

    /// Print every Nth fork's per-fork latency for visibility.
    #[arg(long, default_value_t = 10)]
    progress_every: usize,

    /// Density mode: spin up this many forks and keep them alive while we
    /// sample host RSS + Pss. `0` skips the density phase. The Pss number
    /// (proportional set size; `/proc/self/smaps_rollup`) is the right
    /// per-fork accounting because pages shared via the snapshot file's
    /// `mmap(MAP_PRIVATE)` count fractionally — exactly the unit
    /// economics of fork-many.
    #[arg(long, default_value_t = 0)]
    alive: usize,

    /// Seconds to let the alive forks settle before sampling memory so
    /// the page cache + guest working set stabilise.
    #[arg(long, default_value_t = 5)]
    settle_secs: u64,
}

#[cfg(feature = "kvm")]
fn main() -> Result<()> {
    let args = Args::parse();
    if args.forks == 0 {
        return Err(anyhow!("--forks must be > 0"));
    }
    if !args.kernel.exists() {
        return Err(anyhow!("kernel not found: {}", args.kernel.display()));
    }
    if !args.initrd.exists() {
        return Err(anyhow!("initrd not found: {}", args.initrd.display()));
    }

    println!(
        "nanovm-fork-bench: kernel={} initrd={}",
        args.kernel.display(),
        args.initrd.display(),
    );
    println!(
        "nanovm-fork-bench: forks={} memory={} MiB",
        args.forks, args.memory_mib,
    );

    let hv = KvmHypervisor::new().context("open /dev/kvm")?;

    // 1) Boot the golden VM and wait for it to reach the tick loop.
    let cfg = VmConfig {
        vcpus: 1,
        memory_mib: args.memory_mib,
        kernel: Some(args.kernel.clone()),
        initrd: Some(args.initrd.clone()),
        cmdline: "console=ttyS0,115200 panic=-1 rdinit=/init NANOVM_AGENT_TICK=1".into(),
        ..VmConfig::default()
    };
    let golden = hv.create_vm(&cfg).context("create golden VM")?;
    hv.start(golden.id).context("start golden VM")?;

    let warm = Instant::now();
    let warm_deadline = warm + Duration::from_secs(args.warm_secs);
    let warm_serial = loop {
        let s = hv
            .serial_output(golden.id)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        if s.contains("nanovm-tick") {
            break s;
        }
        if Instant::now() >= warm_deadline {
            let _ = hv.stop(golden.id);
            let _ = hv.destroy(golden.id);
            return Err(anyhow!(
                "golden VM never reached tick mode within {}s\n  serial:\n{s}",
                args.warm_secs,
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let warm_ms = warm.elapsed().as_millis();
    println!(
        "nanovm-fork-bench: golden warmed to tick mode in {warm_ms} ms ({} bytes of serial)",
        warm_serial.len()
    );

    // 2) Snapshot it once.
    let t = Instant::now();
    let snap = hv.snapshot(golden.id).context("snapshot golden VM")?;
    let snap_ms = t.elapsed().as_millis();
    println!("nanovm-fork-bench: snapshot taken ({snap}, {snap_ms} ms)");

    // 3) Fork N times sequentially. Each fork is destroyed before the next
    //    is started so the next restore measures cold-start latency, not
    //    contention with N busy-spinning siblings.
    let rss_before_kib = rss_kib();
    let mut latencies: Vec<Duration> = Vec::with_capacity(args.forks);
    let bench_start = Instant::now();
    for i in 0..args.forks {
        let t = Instant::now();
        let fork = hv
            .restore(snap)
            .with_context(|| format!("fork #{i} restore"))?;
        let lat = t.elapsed();
        latencies.push(lat);
        finalize(&hv, fork.id);
        if args.progress_every > 0 && (i + 1) % args.progress_every == 0 {
            eprintln!("  ... forked {}/{} (last: {:?})", i + 1, args.forks, lat);
        }
    }
    let total = bench_start.elapsed();
    let rss_after_kib = rss_kib();

    // 4) Latency report.
    print_results(&latencies, total, rss_before_kib, rss_after_kib);

    // 5) Density phase: spin up `--alive N` forks, keep them alive, sample.
    if args.alive > 0 {
        run_density_phase(&hv, snap, args.alive, args.settle_secs)?;
    }

    // 6) Cleanup.
    finalize(&hv, golden.id);
    let _ = hv.delete_snapshot(snap);

    Ok(())
}

/// Spin up `n` forks, keep them alive for `settle_secs`, sample the host's
/// RSS + Pss, then tear them down. Reports per-fork Pss (the
/// page-cache-aware accounting) plus the "savings vs. naive N × baseline"
/// — the headline density number.
#[cfg(feature = "kvm")]
fn run_density_phase(
    hv: &KvmHypervisor,
    snap: vm_core::SnapshotId,
    n: usize,
    settle_secs: u64,
) -> Result<()> {
    println!();
    println!("=== density phase ===");
    println!("forks alive: {n}");
    let baseline_rss = rss_kib();
    let baseline_pss = pss_kib();

    let mut alive = Vec::with_capacity(n);
    let phase_start = Instant::now();
    for i in 0..n {
        let fork = hv
            .restore(snap)
            .with_context(|| format!("density fork #{i} restore"))?;
        alive.push(fork);
    }
    let restore_total = phase_start.elapsed();

    println!("settling for {settle_secs}s while {n} forks busy-spin...");
    std::thread::sleep(Duration::from_secs(settle_secs));

    let after_rss = rss_kib();
    let after_pss = pss_kib();

    // Tear the forks down before printing — keeps the host clean even if
    // the report panics.
    for h in &alive {
        finalize(hv, h.id);
    }

    println!();
    println!("alive forks restore total: {restore_total:?}");
    print_density(n, baseline_rss, after_rss, baseline_pss, after_pss);
    Ok(())
}

/// Pretty-print the density numbers, including the shared-page savings
/// ratio — the headline product number for "how many sandboxes fit".
#[cfg(feature = "kvm")]
fn print_density(
    n: usize,
    baseline_rss_kib: Option<u64>,
    after_rss_kib: Option<u64>,
    baseline_pss_kib: Option<u64>,
    after_pss_kib: Option<u64>,
) {
    let report = |label: &str, before: Option<u64>, after: Option<u64>| -> Option<f64> {
        match (before, after) {
            (Some(b), Some(a)) => {
                let delta = a as i64 - b as i64;
                let per_fork_kib = delta as f64 / n as f64;
                let per_fork_mib = per_fork_kib / 1024.0;
                println!(
                    "{label:>7}: before {} KiB → after {} KiB (Δ {:+} KiB; per fork {:.1} KiB = {:.2} MiB)",
                    b, a, delta, per_fork_kib, per_fork_mib,
                );
                Some(per_fork_kib)
            }
            _ => {
                println!("{label:>7}: (unavailable)");
                None
            }
        }
    };
    let per_fork_rss = report("RSS", baseline_rss_kib, after_rss_kib);
    let per_fork_pss = report("Pss", baseline_pss_kib, after_pss_kib);

    if let (Some(rss), Some(pss)) = (per_fork_rss, per_fork_pss) {
        if rss > 0.0 {
            let savings = (rss - pss) / rss * 100.0;
            println!();
            println!(
                "shared-page savings (RSS → Pss): {savings:.1}%  \
                 (per-fork Pss {pss:.1} KiB vs. RSS {rss:.1} KiB)"
            );
        }
        // Project density: how many fit in 16 GiB after subtracting the
        // baseline (kernel + control-plane + golden VM).
        if let (Some(base), Some(per)) = (baseline_pss_kib, Some(pss)) {
            let host_budget_kib: f64 = 16.0 * 1024.0 * 1024.0;
            let usable_kib = host_budget_kib - base as f64;
            if per > 0.0 {
                let fits = (usable_kib / per).floor();
                println!(
                    "projection (16 GiB host, Pss accounting): \
                     ~{fits:.0} concurrent forks fit"
                );
            }
        }
    }
}

/// Read this process's Pss (proportional set size, accounting for shared
/// pages fractionally) from `/proc/self/smaps_rollup`. `None` if the file
/// or field is missing (kernels < 4.14 didn't expose this).
#[cfg(feature = "kvm")]
fn pss_kib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/smaps_rollup").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("Pss:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

#[cfg(feature = "kvm")]
fn finalize(hv: &KvmHypervisor, id: VmId) {
    let _ = hv.stop(id);
    let _ = hv.destroy(id);
}

#[cfg(feature = "kvm")]
fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Read this process's resident-set size from `/proc/self/status` in KiB.
/// `None` if the file is unavailable or the field is missing.
#[cfg(feature = "kvm")]
fn rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

#[cfg(feature = "kvm")]
fn print_results(
    latencies: &[Duration],
    total: Duration,
    rss_before_kib: Option<u64>,
    rss_after_kib: Option<u64>,
) {
    let n = latencies.len();
    let mut sorted = latencies.to_vec();
    sorted.sort();

    let min = sorted.first().copied().unwrap_or(Duration::ZERO);
    let max = sorted.last().copied().unwrap_or(Duration::ZERO);
    let mean = if n == 0 {
        Duration::ZERO
    } else {
        sorted.iter().sum::<Duration>() / (n as u32)
    };
    let p50 = percentile(&sorted, 0.50);
    let p95 = percentile(&sorted, 0.95);
    let p99 = percentile(&sorted, 0.99);
    let throughput = if total.as_secs_f64() > 0.0 {
        n as f64 / total.as_secs_f64()
    } else {
        0.0
    };

    println!();
    println!("=== nanovm-fork-bench results ===");
    println!("forks:           {n}");
    println!("total wall time: {total:?}");
    println!("throughput:      {throughput:.1} forks/sec");
    println!();
    println!("fork latency (restore from snapshot → Running VM handle):");
    println!("  min:  {min:?}");
    println!("  p50:  {p50:?}");
    println!("  p95:  {p95:?}");
    println!("  p99:  {p99:?}");
    println!("  max:  {max:?}");
    println!("  mean: {mean:?}");
    println!();
    println!("host process RSS:");
    match (rss_before_kib, rss_after_kib) {
        (Some(b), Some(a)) => {
            let delta = a as i64 - b as i64;
            println!("  before: {} KiB ({:.1} MiB)", b, b as f64 / 1024.0);
            println!("  after:  {} KiB ({:.1} MiB)", a, a as f64 / 1024.0);
            println!("  delta:  {delta:+} KiB");
            println!(
                "  note: forks are destroyed sequentially, so the delta is residual page-cache / \
                 allocator state, not per-fork footprint. Use a long-running pool benchmark to \
                 measure the latter."
            );
        }
        _ => println!("  (unavailable — /proc/self/status not readable)"),
    }
}
