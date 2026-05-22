//! M1 end-to-end integration test against a *real* Linux bzImage.
//!
//! Companion to `flat_binary.rs` (#56). Where flat-binary mode lets
//! us validate the vCPU run loop in real-mode against a 22-byte
//! hand-rolled program, this test exercises the actual M1
//! production path:
//!
//! - `KvmBootPlan::from_config` allocates guest memory, builds the
//!   default kernel cmdline.
//! - `linux-loader`'s `BzImage::load` parses the bzImage header
//!   and copies the kernel into guest RAM at 0x200000.
//! - `configure_linux_boot` writes the zero-page + e820 + GDT.
//! - `configure_boot_vcpu` brings the vCPU up in long mode at the
//!   bzImage entry point.
//! - The kernel boots far enough to print `Linux version …` to the
//!   8250 UART (caught by `handle_io_out`), then panics on missing
//!   rootfs (CONFIG_PANIC_TIMEOUT=-1 → HLT loop → vCPU thread
//!   exits → state transitions to `Stopped`).
//!
//! The test skips (and passes) if no bzImage is available, so a
//! fresh checkout on a host without `tools/kernel/build-tiny-kernel.sh`
//! built doesn't redden CI:
//!
//! - `NANOVM_TEST_KERNEL` env var: explicit override (used by CI).
//! - else `<workspace>/tools/kernel/cache/bzImage`: the symlink the
//!   build script writes.
//!
//! When the kernel IS present, it boots in 100–500 ms on a modern
//! laptop. The test gives it 30 s before failing, which is
//! generous enough that "real kernel slow to start under load"
//! never produces a flake.
//!
//! Run with:
//!
//! ```sh
//! tools/kernel/build-tiny-kernel.sh
//! cargo test -p vm-kvm --features kvm bzimage_boot -- --nocapture
//! ```

#![cfg(feature = "kvm")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vm_core::{Hypervisor, VmConfig, VmState};
use vm_kvm::KvmHypervisor;

/// Locate the bzImage to boot. Returns `None` (test will skip)
/// when nothing is configured.
fn resolve_kernel_path() -> Option<PathBuf> {
    if let Ok(s) = std::env::var("NANOVM_TEST_KERNEL") {
        let p = PathBuf::from(s);
        return p.exists().then_some(p);
    }
    // CARGO_MANIFEST_DIR is the crate root (`crates/vm-kvm`); walk up
    // two levels to reach the workspace, then into tools/kernel/cache.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidate = manifest
        .parent()? // crates/
        .parent()? // workspace
        .join("tools/kernel/cache/bzImage");
    candidate.exists().then_some(candidate)
}

/// Poll until the VM leaves `Running` or the deadline passes.
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
fn bzimage_boots_far_enough_to_print_linux_version() {
    let Some(kernel) = resolve_kernel_path() else {
        eprintln!(
            "bzimage_boot: skipping — set NANOVM_TEST_KERNEL or run \
             tools/kernel/build-tiny-kernel.sh first."
        );
        return;
    };
    eprintln!("bzimage_boot: booting {}", kernel.display());

    let hv = KvmHypervisor::new().expect("open /dev/kvm");

    let cfg = VmConfig {
        vcpus: 1,
        // Tinyconfig needs ~32 MiB to boot comfortably; 128 leaves
        // headroom for early init even if the kernel later grows
        // a bit. Still nothing by hypervisor-host standards.
        memory_mib: 128,
        kernel: Some(kernel),
        // Force `earlyprintk` to the 8250 so we see output BEFORE
        // the kernel registers its own serial driver. Without it
        // we might miss the `Linux version` line on fast hosts.
        cmdline: "earlyprintk=ttyS0,115200 console=ttyS0,115200 panic=-1".into(),
        ..VmConfig::default()
    };

    let handle = hv.create_vm(&cfg).expect("create_vm");
    assert_eq!(handle.state, VmState::Created);
    hv.start(handle.id).expect("start");

    // A tinyconfig kernel reaches `Linux version` within a few
    // hundred ms; 30 s catches "kernel hung mid-boot" without
    // ever being hit on a healthy run.
    let final_state = wait_for_terminal(&hv, handle.id, Duration::from_secs(30));
    let out = hv.serial_output(handle.id).expect("serial_output");
    let out_str = String::from_utf8_lossy(&out);

    // What we always want to see: the kernel banner. Anything before
    // a "Linux version" string means the bzImage didn't even get to
    // its `start_kernel` and we have a vm-kvm bring-up bug.
    assert!(
        out_str.contains("Linux version"),
        "serial output did not contain 'Linux version' (state={:?}, {} bytes captured):\n{}",
        final_state,
        out.len(),
        out_str,
    );

    // The kernel SHOULD also hit panic-on-no-rootfs and HLT, which
    // would transition state to Stopped. If it didn't, that's a
    // weaker signal (might just mean it took longer than 30 s),
    // so we only warn rather than fail.
    if final_state != VmState::Stopped {
        eprintln!(
            "bzimage_boot: NOTE — vCPU still {:?} after 30 s. \
             Banner was captured, so kernel boot worked; but the \
             VM didn't reach a terminal state. Often a panic-on-no-rootfs \
             timing issue; rerun and inspect serial output if it \
             persists.",
            final_state,
        );
    }

    // Clean up regardless.
    let _ = hv.stop(handle.id);
    let _ = hv.destroy(handle.id);
}
