//! M2/D1 integration test: boot the tiny kernel **with an initramfs**
//! and prove a userspace process runs inside the guest.
//!
//! Builds on the bzImage boot test (M1) by additionally loading an
//! initramfs whose `/init` prints a known marker to the serial
//! console. Asserting that marker proves the whole chain works:
//!
//! - `vm-kvm` loads the initramfs into guest RAM and points the boot
//!   params' ramdisk fields at it (`load_initrd` + `configure_linux_boot`).
//! - The kernel unpacks the initramfs as its root filesystem.
//! - The kernel wires PID 1's stdio to `console=ttyS0` (the
//!   `/dev/console` node ships in the cpio).
//! - `/init` executes in **guest userspace** and writes the marker,
//!   which reaches the host's `serial_output` capture.
//!
//! This is the prerequisite for running the guest agent (next M2
//! step): if a userspace `/init` runs and talks to the host, the
//! agent can too.
//!
//! Skips (and passes) when the kernel or initramfs fixtures aren't
//! built — see `tools/kernel/build-tiny-kernel.sh` and
//! `tools/initramfs/build-initramfs.sh`.
//!
//! Run with:
//!
//! ```sh
//! tools/kernel/build-tiny-kernel.sh
//! tools/initramfs/build-initramfs.sh
//! cargo test -p vm-kvm --features kvm initramfs_boot -- --nocapture
//! ```

#![cfg(feature = "kvm")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vm_core::{Hypervisor, VmConfig, VmState};
use vm_kvm::KvmHypervisor;

/// The marker `tools/initramfs/init.c` writes to the console.
const GUEST_MARKER: &str = "GUEST_USERSPACE_OK";

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/vm-kvm; up two = workspace.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("workspace root")
}

/// Resolve a fixture from an env override or a workspace-relative
/// default. Returns `None` (test skips) when neither exists.
fn resolve(env_key: &str, default_rel: &str) -> Option<PathBuf> {
    if let Ok(s) = std::env::var(env_key) {
        let p = PathBuf::from(s);
        return p.exists().then_some(p);
    }
    let p = workspace_root().join(default_rel);
    p.exists().then_some(p)
}

fn wait_for_terminal(hv: &KvmHypervisor, id: vm_core::VmId, timeout: Duration) -> VmState {
    let deadline = Instant::now() + timeout;
    loop {
        let st = hv.state(id).expect("query state");
        if st != VmState::Running || Instant::now() >= deadline {
            return st;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn initramfs_runs_guest_userspace_init() {
    let Some(kernel) = resolve("NANOVM_TEST_KERNEL", "tools/kernel/cache/bzImage") else {
        eprintln!("initramfs_boot: skipping — run tools/kernel/build-tiny-kernel.sh first.");
        return;
    };
    let Some(initrd) = resolve(
        "NANOVM_TEST_INITRAMFS",
        "tools/initramfs/cache/initramfs.cpio",
    ) else {
        eprintln!("initramfs_boot: skipping — run tools/initramfs/build-initramfs.sh first.");
        return;
    };
    eprintln!(
        "initramfs_boot: kernel={} initrd={}",
        kernel.display(),
        initrd.display(),
    );

    let hv = KvmHypervisor::new().expect("open /dev/kvm");
    let cfg = VmConfig {
        vcpus: 1,
        memory_mib: 128,
        kernel: Some(kernel),
        initrd: Some(initrd),
        // `rdinit=/init` is the default for an initramfs, but be
        // explicit; console=ttyS0 routes printk + init stdio to COM1.
        cmdline: "console=ttyS0,115200 panic=-1 rdinit=/init".into(),
        ..VmConfig::default()
    };

    let handle = hv.create_vm(&cfg).expect("create_vm");
    hv.start(handle.id).expect("start");

    // The kernel boots + unpacks the initramfs + runs init in well
    // under a second on a modern host; 30 s catches a hang without
    // ever tripping on a healthy run.
    let final_state = wait_for_terminal(&hv, handle.id, Duration::from_secs(30));
    let out = hv.serial_output(handle.id).expect("serial_output");
    let out_str = String::from_utf8_lossy(&out);
    let run_err = hv.last_run_error(handle.id).ok().flatten();

    assert!(
        out_str.contains(GUEST_MARKER),
        "guest userspace marker {GUEST_MARKER:?} not found in serial output\n  \
         state={:?}\n  bytes captured={}\n  last_run_error={:?}\n  serial:\n{}",
        final_state,
        out.len(),
        run_err,
        out_str,
    );

    let _ = hv.stop(handle.id);
    let _ = hv.destroy(handle.id);
}
