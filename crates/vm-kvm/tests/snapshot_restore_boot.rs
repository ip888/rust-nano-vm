//! M5/S1b integration test: snapshot a running guest and restore it into a
//! fresh VM that **resumes**.
//!
//! This is the first end-to-end exercise of "snapshot once, fork many": a
//! guest is captured mid-execution (vCPU registers + MSRs + LAPIC + the
//! in-kernel PIC/IOAPIC/PIT/clock + all of guest RAM), then a brand-new VM
//! is rebuilt from that snapshot and run. If the restore is faithful, the
//! restored guest keeps executing exactly where the original left off.
//!
//! The guest runs the agent's **tick mode** (`NANOVM_AGENT_TICK=1`): a
//! busy-spin loop that logs `nanovm-tick <n>` to the serial console and
//! never HLT-idles (an idle guest would exit and stop the VM). Because the
//! counter lives in guest RAM and the loop's rip in vCPU state, a faithful
//! restore continues emitting ticks — observable on the *restored* VM's
//! serial. No vsock device is attached (device-state capture lands later),
//! so the snapshot path is exercised on a plain guest.
//!
//! Skips (and passes) without the fixtures:
//!   tools/kernel/build-tiny-kernel.sh
//!   NANOVM_INIT=agent tools/initramfs/build-initramfs.sh
//!
//! Run with:
//!
//! ```sh
//! cargo test -p vm-kvm --features kvm --test snapshot_restore_boot -- --nocapture
//! ```

#![cfg(feature = "kvm")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vm_core::{Hypervisor, VmConfig, VmId, VmState};
use vm_kvm::KvmHypervisor;

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

/// Poll a VM's serial output until it contains `needle` or the deadline
/// passes. Returns the captured output (which may or may not contain it).
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
fn snapshot_then_restore_resumes_the_guest() {
    let Some(kernel) = resolve("NANOVM_TEST_KERNEL", "tools/kernel/cache/bzImage") else {
        eprintln!("snapshot_restore_boot: skipping — run tools/kernel/build-tiny-kernel.sh first.");
        return;
    };
    let Some(initrd) = resolve(
        "NANOVM_TEST_AGENT_INITRAMFS",
        "tools/initramfs/cache/initramfs-agent.cpio",
    ) else {
        eprintln!(
            "snapshot_restore_boot: skipping — run \
             `NANOVM_INIT=agent tools/initramfs/build-initramfs.sh` first."
        );
        return;
    };
    eprintln!(
        "snapshot_restore_boot: kernel={} initrd={}",
        kernel.display(),
        initrd.display(),
    );

    let hv = KvmHypervisor::new().expect("open /dev/kvm");
    // No vsock_cid: snapshot of a vsock device isn't supported yet. Tick mode
    // keeps the guest busy (no HLT) and emits an observable counter.
    let cfg = VmConfig {
        vcpus: 1,
        memory_mib: 128,
        kernel: Some(kernel),
        initrd: Some(initrd),
        cmdline: "console=ttyS0,115200 panic=-1 rdinit=/init NANOVM_AGENT_TICK=1".into(),
        ..VmConfig::default()
    };

    let original = hv.create_vm(&cfg).expect("create_vm");
    hv.start(original.id).expect("start");

    // Let the guest boot and emit a few ticks so the snapshot captures it
    // mid-loop with a non-zero counter.
    let serial = wait_for_serial(&hv, original.id, "nanovm-tick 1", Duration::from_secs(40));
    assert!(
        serial.contains("nanovm-tick"),
        "guest never reached tick mode; can't snapshot a running loop.\n  serial:\n{serial}"
    );

    // Capture, then immediately restore into a fresh VM.
    let snap = hv.snapshot(original.id).unwrap_or_else(|e| {
        let _ = hv.stop(original.id);
        let _ = hv.destroy(original.id);
        panic!("snapshot failed: {e}");
    });
    eprintln!("snapshot_restore_boot: captured {snap}");

    // The original keeps running across a (non-destructive) snapshot.
    assert_eq!(
        hv.state(original.id).expect("state"),
        VmState::Running,
        "original VM should keep running after a snapshot"
    );

    let restored = hv.restore(snap).unwrap_or_else(|e| {
        let _ = hv.stop(original.id);
        let _ = hv.destroy(original.id);
        panic!("restore failed: {e}");
    });
    eprintln!("snapshot_restore_boot: restored as {}", restored.id);

    // The restored guest must resume the tick loop — proving the vCPU rip,
    // the in-RAM counter, and the machine state all came back faithfully.
    let restored_serial = wait_for_serial(&hv, restored.id, "nanovm-tick", Duration::from_secs(30));
    let restored_state = hv.state(restored.id).expect("state");

    let _ = hv.stop(original.id);
    let _ = hv.destroy(original.id);
    let _ = hv.stop(restored.id);
    let _ = hv.destroy(restored.id);
    let _ = hv.delete_snapshot(snap);

    assert!(
        restored_serial.contains("nanovm-tick"),
        "restored guest produced no ticks — it didn't resume.\n  \
         state={restored_state:?}\n  serial:\n{restored_serial}"
    );
    eprintln!("snapshot_restore_boot: restored guest resumed and emitted ticks");
}
