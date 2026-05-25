//! M2/D3 slice-2 integration test: prove the guest **discovers**
//! vm-kvm's virtio-vsock device over the virtio-MMIO transport.
//!
//! With `VmConfig.vsock_cid` set, vm-kvm:
//!   - instantiates an `MmioTransport` (the device register model),
//!   - exposes it at the `VSOCK_MMIO_BASE` register window (MMIO
//!     exits in that window are routed into it), and
//!   - appends `virtio_mmio.device=<size>@<base>:<irq>` to the
//!     kernel cmdline.
//!
//! During early boot (before `/init`), the guest's `virtio_mmio`
//! driver parses that param, reads our Magic/Version/DeviceID
//! registers, recognizes a virtio device, and the virtio core writes
//! `ACKNOWLEDGE | DRIVER` into the status register. We observe that
//! host-side via `KvmHypervisor::vsock_status` — a non-zero status
//! with the `ACKNOWLEDGE` bit set proves the guest found and began
//! driving our device. (The full vsock data path — virtqueues +
//! the in-guest VSOCKETS driver — lands in the next slice.)
//!
//! Skips (and passes) without the kernel + initramfs fixtures. Needs
//! a kernel built with CONFIG_VIRTIO_MMIO[_CMDLINE_DEVICES] (the
//! current tinyconfig.fragment) — rebuild via
//! `tools/kernel/build-tiny-kernel.sh` after pulling this branch.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p vm-kvm --features kvm --test vsock_probe_boot -- --nocapture
//! ```

#![cfg(feature = "kvm")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vm_core::{Hypervisor, VmConfig};
use vm_kvm::KvmHypervisor;

/// `STATUS_ACKNOWLEDGE` from the virtio spec — the first bit the
/// guest sets once it recognizes the device.
const STATUS_ACKNOWLEDGE: u32 = 1;

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

#[test]
fn guest_probes_virtio_vsock_device() {
    let Some(kernel) = resolve("NANOVM_TEST_KERNEL", "tools/kernel/cache/bzImage") else {
        eprintln!("vsock_probe_boot: skipping — run tools/kernel/build-tiny-kernel.sh first.");
        return;
    };
    // Any initramfs works — the virtio_mmio probe runs during kernel
    // init, before /init. The C test fixture is the simplest.
    let Some(initrd) = resolve(
        "NANOVM_TEST_INITRAMFS",
        "tools/initramfs/cache/initramfs.cpio",
    ) else {
        eprintln!("vsock_probe_boot: skipping — run tools/initramfs/build-initramfs.sh first.");
        return;
    };
    eprintln!(
        "vsock_probe_boot: kernel={} initrd={}",
        kernel.display(),
        initrd.display(),
    );

    let hv = KvmHypervisor::new().expect("open /dev/kvm");
    let cfg = VmConfig {
        vcpus: 1,
        memory_mib: 128,
        kernel: Some(kernel),
        initrd: Some(initrd),
        // Setting vsock_cid is what makes vm-kvm attach the device
        // and append the virtio_mmio.device= cmdline param.
        vsock_cid: Some(3),
        cmdline: "console=ttyS0,115200 panic=-1 rdinit=/init".into(),
        ..VmConfig::default()
    };

    let handle = hv.create_vm(&cfg).expect("create_vm");
    hv.start(handle.id).expect("start");

    // Poll the device status. The guest sets ACKNOWLEDGE during the
    // early virtio_mmio probe; status persists in the host-side
    // transport even after the guest reboots. 30 s is generous.
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        let s = hv
            .vsock_status(handle.id)
            .expect("vsock_status")
            .expect("vsock device should exist when vsock_cid is set");
        if s & STATUS_ACKNOWLEDGE != 0 || Instant::now() >= deadline {
            break s;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let serial = hv
        .serial_output(handle.id)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let _ = hv.stop(handle.id);
    let _ = hv.destroy(handle.id);

    assert!(
        status & STATUS_ACKNOWLEDGE != 0,
        "guest did not ACKNOWLEDGE the virtio-vsock device \
         (status={status:#x}); the virtio_mmio driver never probed it.\n  serial:\n{serial}",
    );
    eprintln!("vsock_probe_boot: device status = {status:#x} (ACKNOWLEDGE set)");
}
