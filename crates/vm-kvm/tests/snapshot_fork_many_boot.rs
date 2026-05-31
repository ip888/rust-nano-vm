//! M5/S2 integration test: **snapshot once, fork many** via `MAP_PRIVATE`
//! CoW on the snapshot memory image.
//!
//! Restoring a snapshot now `mmap(MAP_PRIVATE, fd, …)`s the memory backing
//! file directly — the kernel page-cache shares all unmodified pages
//! across forks, and each fork's writes go to its own private anonymous
//! pages on first touch. The cold-start cost drops from "128 MiB memcpy"
//! to one `mmap` syscall, and N forks share memory at the page granularity
//! of whatever they actually dirty — the unit-economics of an AI sandbox
//! platform.
//!
//! Concretely: take one snapshot of the tick guest, then `restore()` it N
//! times. Every fork must independently emit `nanovm-tick`s; failure means
//! either fork lifecycle isn't isolated (one fork's writes leaked into
//! another) or memory wasn't faulted in.
//!
//! Skips (and passes) without the fixtures:
//!   tools/kernel/build-tiny-kernel.sh
//!   NANOVM_INIT=agent tools/initramfs/build-initramfs.sh
//!
//! Run with:
//!
//! ```sh
//! cargo test -p vm-kvm --features kvm --test snapshot_fork_many_boot -- --nocapture
//! ```

#![cfg(feature = "kvm")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vm_core::{Hypervisor, VmConfig, VmId};
use vm_kvm::KvmHypervisor;

const FORK_COUNT: usize = 3;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("workspace root")
}

fn resolve(env_key: &str, default_rel: &str) -> Option<PathBuf> {
    if let Ok(s) = std::env::var(env_key) {
        let p = PathBuf::from(s);
        return p.exists().then_some(p);
    }
    let p = workspace_root().join(default_rel);
    p.exists().then_some(p)
}

fn wait_for_serial(hv: &KvmHypervisor, id: VmId, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let s = hv
            .serial_output(id)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        if s.contains(needle) || Instant::now() >= deadline {
            return s;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn snapshot_then_fork_many_times_each_emits_ticks() {
    let Some(kernel) = resolve("NANOVM_TEST_KERNEL", "tools/kernel/cache/bzImage") else {
        eprintln!(
            "snapshot_fork_many_boot: skipping — run tools/kernel/build-tiny-kernel.sh first."
        );
        return;
    };
    let Some(initrd) = resolve(
        "NANOVM_TEST_AGENT_INITRAMFS",
        "tools/initramfs/cache/initramfs-agent.cpio",
    ) else {
        eprintln!(
            "snapshot_fork_many_boot: skipping — run \
             `NANOVM_INIT=agent tools/initramfs/build-initramfs.sh` first."
        );
        return;
    };

    let hv = KvmHypervisor::new().expect("open /dev/kvm");
    let cfg = VmConfig {
        vcpus: 1,
        memory_mib: 128,
        kernel: Some(kernel),
        initrd: Some(initrd),
        cmdline: "console=ttyS0,115200 panic=-1 rdinit=/init NANOVM_AGENT_TICK=1".into(),
        ..VmConfig::default()
    };

    // Boot, let it reach the tick loop, take ONE snapshot.
    let original = hv.create_vm(&cfg).expect("create_vm");
    hv.start(original.id).expect("start original");
    let serial = wait_for_serial(&hv, original.id, "nanovm-tick 1", Duration::from_secs(40));
    assert!(
        serial.contains("nanovm-tick"),
        "guest never reached tick mode.\n  serial:\n{serial}"
    );
    let snap = hv.snapshot(original.id).expect("snapshot");
    eprintln!("snapshot_fork_many_boot: captured {snap}");

    // Fork N times off the same snapshot, recording per-fork latency.
    let mut forks = Vec::with_capacity(FORK_COUNT);
    for i in 0..FORK_COUNT {
        let started = Instant::now();
        let fork = hv.restore(snap).unwrap_or_else(|e| {
            let _ = hv.stop(original.id);
            let _ = hv.destroy(original.id);
            panic!("fork {i} failed: {e}");
        });
        let elapsed = started.elapsed();
        eprintln!(
            "snapshot_fork_many_boot: fork #{i} = {} (restore in {elapsed:?})",
            fork.id
        );
        forks.push(fork);
    }

    // Each fork must independently produce its own ticks.
    let mut all_serials = Vec::with_capacity(FORK_COUNT);
    for fork in &forks {
        let s = wait_for_serial(&hv, fork.id, "nanovm-tick", Duration::from_secs(30));
        all_serials.push((fork.id, s));
    }

    // Tear everything down before asserting so the host is left clean.
    let _ = hv.stop(original.id);
    let _ = hv.destroy(original.id);
    for fork in &forks {
        let _ = hv.stop(fork.id);
        let _ = hv.destroy(fork.id);
    }
    let _ = hv.delete_snapshot(snap);

    for (id, s) in &all_serials {
        assert!(
            s.contains("nanovm-tick"),
            "fork {id} produced no ticks — CoW isolation or fault path is broken.\n  \
             serial:\n{s}"
        );
    }
    eprintln!(
        "snapshot_fork_many_boot: all {FORK_COUNT} forks resumed and emitted ticks independently"
    );
}
