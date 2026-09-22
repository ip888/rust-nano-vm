//! `nanovm-jvm-bench` — snapshot / fork latency benchmark for the
//! Petclinic Milestone-1 prototype.
//!
//! Boots the Java rootfs (produced by `tools/java-rootfs/build.sh`)
//! as an initramfs guest, waits for the in-guest warmup driver to
//! emit `NANOVM_PETCLINIC_READY` on the serial console, snapshots
//! the guest, then forks the snapshot N times and measures the
//! per-fork "time to a ready guest" latency.
//!
//! ## Cold vs warm snapshot
//!
//! The bench supports three modes via `--snapshot-at`:
//!
//! - **`warm`** (default) — snapshot AFTER the ready marker. Each
//!   restored fork is instantly serving; the reported latency is just
//!   the `restore()` cost. This is the marketing number.
//!
//! - **`cold`** — snapshot as soon as the guest kernel finishes boot
//!   (before the ready marker). Each forked child still has to
//!   complete Spring context init + warmup after the restore, so the
//!   reported latency is `restore()` + wait-for-marker in the child.
//!   Verifies that the fork mechanism preserves an in-progress JVM
//!   heap correctly.
//!
//! - **`both`** — do both runs back-to-back, print a side-by-side
//!   comparison. Uses two snapshots on the same golden VM.
//!
//! ## Build
//!
//! The bench needs `/dev/kvm` at run time and the `kvm` feature at
//! compile time:
//!
//!     cargo run -p bench --release --features kvm --bin nanovm-jvm-bench -- \
//!         --kernel    tools/kvm-images/cache/vmlinux \
//!         --initramfs tools/java-rootfs/cache/initramfs.cpio.gz \
//!         --memory-mib 2048 \
//!         --snapshot-at both \
//!         --forks 20
//!
//! Without `--features kvm` the binary builds (so it lands in the
//! default workspace checks) but refuses to run — same posture as
//! `nanovm-fork-bench`.

#[cfg(not(feature = "kvm"))]
fn main() {
    eprintln!(
        "nanovm-jvm-bench: build with --features kvm, e.g. \
         `cargo run -p bench --release --features kvm --bin nanovm-jvm-bench`",
    );
    std::process::exit(2);
}

#[cfg(feature = "kvm")]
mod inner {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, bail, Context, Result};
    use clap::{Parser, ValueEnum};
    use vm_core::{Hypervisor, SnapshotId, VmConfig, VmId};
    use vm_kvm::KvmHypervisor;

    // `serial_output` is a KVM-specific helper, not part of the
    // `Hypervisor` trait, so every function that needs to read guest
    // console output takes `&KvmHypervisor` (concrete) rather than
    // `&dyn Hypervisor`. Everything else could use the trait, but
    // there's no other backend in scope here, so the whole binary
    // stays on the concrete type for simplicity.

    /// The exact console marker `warmup.sh` emits when Spring Boot is
    /// warmed and ready. Any change here MUST match the guest side —
    /// see `tools/java-rootfs/warmup.sh`.
    const READY_MARKER: &str = "NANOVM_PETCLINIC_READY";

    /// The kernel-cmdline marker the guest kernel prints on any boot
    /// (init.sh's first stderr line). The "cold" mode uses it to
    /// tell when the guest is far enough for a snapshot — early
    /// enough that Spring hasn't started, late enough that KVM has
    /// finished bring-up.
    const COLD_MARKER: &str = "[init] launching JVM";

    #[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
    pub(crate) enum SnapshotAt {
        Cold,
        Warm,
        Both,
    }

    #[derive(Parser, Debug)]
    #[command(
        version,
        about = "Measure Petclinic snapshot/fork latency (cold vs warm)"
    )]
    pub(crate) struct Args {
        /// Path to a bzImage Linux kernel.
        #[arg(long, env = "NANOVM_TEST_KERNEL")]
        pub kernel: PathBuf,

        /// Path to the Petclinic initramfs (from
        /// `tools/java-rootfs/build.sh` — the `.cpio.gz` under
        /// `cache/`, not the ext4).
        #[arg(long, env = "NANOVM_TEST_PETCLINIC_INITRAMFS")]
        pub initramfs: PathBuf,

        /// Guest memory (MiB). The initramfs unpacks to ~450 MiB in
        /// guest RAM; Petclinic wants ~1 GiB heap; the kernel +
        /// slack eat the rest. 2048 is comfortable, less than 1536
        /// starts hitting OOM.
        #[arg(long, default_value_t = 2048)]
        pub memory_mib: u64,

        /// Number of measured forks per snapshot mode.
        #[arg(long, default_value_t = 20)]
        pub forks: usize,

        /// Which snapshot(s) to bench.
        #[arg(long, value_enum, default_value_t = SnapshotAt::Warm)]
        pub snapshot_at: SnapshotAt,

        /// Warmup fork count discarded from statistics (the first
        /// restore off a fresh snapshot pays page-cache costs the
        /// rest don't).
        #[arg(long, default_value_t = 3)]
        pub warmup: usize,

        /// Seconds to wait for the guest to reach `COLD_MARKER`
        /// (needed for `--snapshot-at cold` and, indirectly, `both`).
        #[arg(long, default_value_t = 30)]
        pub cold_marker_secs: u64,

        /// Seconds to wait for the guest to reach `READY_MARKER`
        /// (needed for `--snapshot-at warm` and `both`; also used as
        /// the per-fork "wait for ready" cap in `cold` mode).
        #[arg(long, default_value_t = 120)]
        pub warmup_secs: u64,

        /// Print every Nth fork's latency for visibility.
        #[arg(long, default_value_t = 5)]
        pub progress_every: usize,
    }

    pub(crate) fn run() -> Result<()> {
        let args = Args::parse();
        if args.forks == 0 {
            bail!("--forks must be > 0");
        }
        if !args.kernel.exists() {
            bail!("kernel not found: {}", args.kernel.display());
        }
        if !args.initramfs.exists() {
            bail!("initramfs not found: {}", args.initramfs.display());
        }

        println!(
            "nanovm-jvm-bench: kernel={} initramfs={} memory={} MiB forks={} mode={:?}",
            args.kernel.display(),
            args.initramfs.display(),
            args.memory_mib,
            args.forks,
            args.snapshot_at,
        );

        let hv = Arc::new(KvmHypervisor::new().context("open /dev/kvm")?);

        // Boot the golden VM once. We may take up to two snapshots
        // off it (cold + warm) if the mode is Both.
        let cfg = VmConfig {
            vcpus: 1,
            memory_mib: args.memory_mib,
            kernel: Some(args.kernel.clone()),
            initrd: Some(args.initramfs.clone()),
            cmdline: "console=ttyS0,115200 panic=-1 rdinit=/sbin/init".into(),
            ..VmConfig::default()
        };
        let golden = hv.create_vm(&cfg).context("create golden VM")?;
        hv.start(golden.id).context("start golden VM")?;

        let need_cold = matches!(args.snapshot_at, SnapshotAt::Cold | SnapshotAt::Both);
        let need_warm = matches!(args.snapshot_at, SnapshotAt::Warm | SnapshotAt::Both);

        // Wait for the "cold" point (JVM has been exec'd, Spring not
        // yet ready). Guaranteed to precede the ready marker.
        wait_for_marker(&hv, golden.id, COLD_MARKER, args.cold_marker_secs)
            .context("wait for cold marker (JVM launch)")?;

        let cold_snap = if need_cold {
            let t = Instant::now();
            let s = hv
                .snapshot(golden.id)
                .context("cold snapshot of golden VM")?;
            println!(
                "nanovm-jvm-bench: COLD snapshot taken ({s}, {} ms)",
                t.elapsed().as_millis()
            );
            Some(s)
        } else {
            None
        };

        // Continue the golden VM until Spring is warm; this is where
        // the warm snapshot lives. The COLD snapshot above holds the
        // earlier state — the golden VM keeps running.
        let warm_snap = if need_warm {
            wait_for_marker(&hv, golden.id, READY_MARKER, args.warmup_secs)
                .context("wait for ready marker on golden VM")?;
            let t = Instant::now();
            let s = hv
                .snapshot(golden.id)
                .context("warm snapshot of golden VM")?;
            println!(
                "nanovm-jvm-bench: WARM snapshot taken ({s}, {} ms)",
                t.elapsed().as_millis()
            );
            Some(s)
        } else {
            None
        };

        // Cold-phase fork loop.
        let cold_stats = if let Some(snap) = cold_snap {
            Some(bench_snapshot(
                &hv,
                snap,
                "COLD",
                args.forks,
                args.warmup,
                args.progress_every,
                /* wait_ready_secs */ args.warmup_secs,
                /* wait_ready_in_fork */ true,
            )?)
        } else {
            None
        };

        // Warm-phase fork loop.
        let warm_stats = if let Some(snap) = warm_snap {
            Some(bench_snapshot(
                &hv,
                snap,
                "WARM",
                args.forks,
                args.warmup,
                args.progress_every,
                /* wait_ready_secs */ args.warmup_secs,
                /* wait_ready_in_fork */ false,
            )?)
        } else {
            None
        };

        // Side-by-side comparison when both modes ran.
        if let (Some(c), Some(w)) = (cold_stats.as_ref(), warm_stats.as_ref()) {
            print_comparison(c, w);
        }

        // Cleanup.
        finalize(&hv, golden.id);
        if let Some(s) = cold_snap {
            let _ = hv.delete_snapshot(s);
        }
        if let Some(s) = warm_snap {
            let _ = hv.delete_snapshot(s);
        }
        Ok(())
    }

    /// One snapshot's fork loop. Returns per-fork latencies (measured
    /// samples only — warmup samples discarded). When
    /// `wait_ready_in_fork` is true, each fork is followed by a wait
    /// for `READY_MARKER` in the child's serial output; the reported
    /// latency covers both restore and wait.
    #[allow(clippy::too_many_arguments)]
    fn bench_snapshot(
        hv: &KvmHypervisor,
        snap: SnapshotId,
        label: &str,
        forks: usize,
        warmup: usize,
        progress_every: usize,
        wait_ready_secs: u64,
        wait_ready_in_fork: bool,
    ) -> Result<PhaseStats> {
        println!("\n=== {label} phase: {forks} measured forks (+{warmup} warmup) ===",);

        let total_iters = forks + warmup;
        let mut samples: Vec<Duration> = Vec::with_capacity(forks);
        let phase_start = Instant::now();

        for i in 0..total_iters {
            let t = Instant::now();
            let fork = hv
                .restore(snap)
                .with_context(|| format!("{label} fork #{i} restore"))?;
            let restore_lat = t.elapsed();

            let total_lat = if wait_ready_in_fork {
                wait_for_marker(hv, fork.id, READY_MARKER, wait_ready_secs)
                    .with_context(|| format!("{label} fork #{i} wait for ready"))?;
                t.elapsed()
            } else {
                restore_lat
            };

            if i >= warmup {
                samples.push(total_lat);
            }
            finalize(hv, fork.id);

            if progress_every > 0 && (i + 1) % progress_every == 0 {
                eprintln!(
                    "  {label} {}/{}: restore={:?} total={:?}",
                    i + 1,
                    total_iters,
                    restore_lat,
                    total_lat,
                );
            }
        }

        let total = phase_start.elapsed();
        let stats = PhaseStats::from_samples(label.into(), samples, total);
        stats.print_report();
        Ok(stats)
    }

    /// Poll the guest's serial output until `marker` appears or the
    /// deadline passes. Returns Ok(()) when the marker is seen.
    fn wait_for_marker(hv: &KvmHypervisor, vm: VmId, marker: &str, max_secs: u64) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(max_secs);
        loop {
            let s = hv
                .serial_output(vm)
                .ok()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            if s.contains(marker) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "guest never emitted marker {marker:?} within {max_secs}s\n\
                     serial tail (last 4 KiB):\n{}",
                    tail(&s, 4096),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn tail(s: &str, n: usize) -> &str {
        if s.len() <= n {
            s
        } else {
            &s[s.len() - n..]
        }
    }

    /// Best-effort teardown; a leftover error here shouldn't tank
    /// the whole bench run — the OS reclaims the guest memory
    /// anyway on process exit.
    fn finalize(hv: &KvmHypervisor, id: VmId) {
        let _ = hv.stop(id);
        let _ = hv.destroy(id);
    }

    #[derive(Debug)]
    pub(crate) struct PhaseStats {
        label: String,
        n: usize,
        p50: Duration,
        p95: Duration,
        p99: Duration,
        min: Duration,
        max: Duration,
        mean: Duration,
        total: Duration,
    }

    impl PhaseStats {
        fn from_samples(label: String, mut samples: Vec<Duration>, total: Duration) -> Self {
            samples.sort();
            let n = samples.len();
            let min = samples.first().copied().unwrap_or(Duration::ZERO);
            let max = samples.last().copied().unwrap_or(Duration::ZERO);
            let mean = if n == 0 {
                Duration::ZERO
            } else {
                samples.iter().sum::<Duration>() / (n as u32)
            };
            Self {
                label,
                n,
                p50: percentile(&samples, 0.50),
                p95: percentile(&samples, 0.95),
                p99: percentile(&samples, 0.99),
                min,
                max,
                mean,
                total,
            }
        }

        fn print_report(&self) {
            println!("--- {} results ---", self.label);
            println!("  n:      {}", self.n);
            println!("  total:  {:?}", self.total);
            println!("  p50:    {:?}", self.p50);
            println!("  p95:    {:?}", self.p95);
            println!("  p99:    {:?}", self.p99);
            println!("  min:    {:?}", self.min);
            println!("  max:    {:?}", self.max);
            println!("  mean:   {:?}", self.mean);
        }
    }

    fn percentile(sorted: &[Duration], p: f64) -> Duration {
        if sorted.is_empty() {
            return Duration::ZERO;
        }
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    fn print_comparison(cold: &PhaseStats, warm: &PhaseStats) {
        println!("\n=== cold vs warm summary ===");
        println!("| metric | cold | warm | speedup |");
        println!("|--------|------|------|---------|");
        let row = |name: &str, c: Duration, w: Duration| {
            let speedup = if w.as_nanos() > 0 {
                c.as_secs_f64() / w.as_secs_f64()
            } else {
                0.0
            };
            println!("| {name:<6} | {c:?} | {w:?} | {speedup:.1}x |");
        };
        row("p50", cold.p50, warm.p50);
        row("p95", cold.p95, warm.p95);
        row("p99", cold.p99, warm.p99);
        row("mean", cold.mean, warm.mean);
        println!(
            "\nnote: cold latency = restore + wait-for-{READY_MARKER}; \
             warm latency = restore only (guest was already ready at snapshot)."
        );
    }
}

#[cfg(feature = "kvm")]
fn main() -> anyhow::Result<()> {
    inner::run()
}
