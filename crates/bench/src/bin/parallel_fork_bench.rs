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
    /// Number of forks to spawn concurrently.
    #[arg(long, default_value_t = 50)]
    forks: usize,

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
        "nanovm-parallel-fork-bench: kernel={} initrd={}",
        args.kernel.display(),
        args.initrd.display(),
    );
    println!(
        "nanovm-parallel-fork-bench: forks={} memory={} MiB",
        args.forks, args.memory_mib,
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

    // 1. Boot the golden VM.
    let warm_start = Instant::now();
    let golden = boot_golden(&hv, &cfg, &args).context("boot golden")?;
    let warm_ms = warm_start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "nanovm-parallel-fork-bench: golden warmed in {:>7.1} ms",
        warm_ms
    );

    // 2. Snapshot it.
    let snap_start = Instant::now();
    let snap = hv.snapshot(golden.id).context("snapshot golden")?;
    let snap_ms = snap_start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "nanovm-parallel-fork-bench: snapshot taken ({snap}) in {:>7.1} ms",
        snap_ms
    );

    // 3. Golden's vCPU thread is competing with the forks we're
    //    about to spawn — tear it down before the measurement.
    let _ = hv.stop(golden.id);
    let _ = hv.destroy(golden.id);

    let rss_before = rss_kib();

    // 4. Parallel fork-many.
    let (latencies, vm_ids, total) = parallel_restore(Arc::clone(&hv), snap, args.forks);
    let total_ms = total.as_secs_f64() * 1000.0;

    if latencies.is_empty() {
        return Err(anyhow!("every parallel-fork worker failed; see stderr"));
    }
    if latencies.len() < args.forks {
        eprintln!(
            "nanovm-parallel-fork-bench: only {}/{} workers succeeded",
            latencies.len(),
            args.forks,
        );
    }

    let rss_after = rss_kib();

    // 5. Print per-fork distribution.
    let runs = latencies.len();
    let sum_ms: f64 = latencies.iter().map(|d| d.as_secs_f64() * 1000.0).sum();
    let mean_ms = sum_ms / runs as f64;
    let min_ms = latencies[0].as_secs_f64() * 1000.0;
    let max_ms = latencies.last().unwrap().as_secs_f64() * 1000.0;
    let p50_ms = percentile_ms(&latencies, 50.0);
    let p99_ms = percentile_ms(&latencies, 99.0);

    if args.verbose {
        println!();
        println!("nanovm-parallel-fork-bench: per-fork latencies (sorted, ms):");
        for (i, d) in latencies.iter().enumerate() {
            println!("  #{:>4}: {:>7.1} ms", i, d.as_secs_f64() * 1000.0);
        }
    }

    println!();
    println!(
        "nanovm-parallel-fork-bench: {} parallel forks from one snapshot",
        runs
    );
    println!("  total wall-clock       : {:>7.1} ms", total_ms);
    println!();
    println!("  per-fork min           : {:>7.1} ms", min_ms);
    println!("  per-fork p50           : {:>7.1} ms", p50_ms);
    println!("  per-fork mean          : {:>7.1} ms", mean_ms);
    println!("  per-fork p99           : {:>7.1} ms", p99_ms);
    println!("  per-fork max           : {:>7.1} ms", max_ms);

    // 6. The headline: "theoretical sequential" is what you'd get
    //    doing N restore calls back to back (what Firecracker gets
    //    for its `LoadSnapshot` loop). Our wall-clock is the
    //    parallel equivalent.
    let seq_est_ms = mean_ms * runs as f64;
    let speedup = if total_ms > 0.0 {
        seq_est_ms / total_ms
    } else {
        0.0
    };
    println!();
    println!(
        "  theoretical sequential : {:>7.1} ms ({} × mean {:.1} ms)",
        seq_est_ms, runs, mean_ms,
    );
    println!("  parallel wall-clock    : {:>7.1} ms", total_ms);
    println!("  speed-up               : {:>7.1}×", speedup);

    // 7. Memory story — how much per fork, with the shared-page
    //    advantage. `None` when `/proc/self/status` wasn't readable.
    if let (Some(b), Some(a)) = (rss_before, rss_after) {
        let delta = a as i64 - b as i64;
        let per_fork_kib = delta as f64 / runs as f64;
        let per_fork_mib = per_fork_kib / 1024.0;
        println!();
        println!("  host RSS before        : {:>7} KiB", b);
        println!("  host RSS after         : {:>7} KiB", a);
        println!(
            "  Δ host RSS             : {:>+7} KiB (per fork {:.1} KiB = {:.2} MiB)",
            delta, per_fork_kib, per_fork_mib,
        );
    }

    // 8. Teardown — every restored VM, in parallel. Keep the host
    //    clean even if the user re-runs the bench.
    let teardown_handles: Vec<_> = vm_ids
        .into_iter()
        .map(|id| {
            let hv_cloned = Arc::clone(&hv);
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

    Ok(())
}
