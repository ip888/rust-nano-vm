//! M2/D3 slice-3c integration test: prove the guest's **virtio-vsock
//! driver** binds to vm-kvm's device, negotiates features, sets up the
//! rx/tx/event virtqueues, and reaches `DRIVER_OK`.
//!
//! This goes a step beyond `vsock_probe_boot` (slice 2), which only
//! proved the generic virtio-MMIO core *probed* the device and set
//! `ACKNOWLEDGE`. Reaching `DRIVER_OK` requires a real in-guest
//! virtio-vsock driver (`CONFIG_VIRTIO_VSOCKETS`, which pulls in
//! `VSOCKETS` + `NET`) that programs the queues through our MMIO
//! register window. We observe it host-side via
//! `KvmHypervisor::vsock_driver_ok`.
//!
//! During its probe the guest driver writes the queue descriptor/avail/
//! used addresses into our [`MmioTransport`] registers and kicks the rx
//! queue; vm-kvm routes those MMIO exits into the `VsockDevice` and runs
//! one device cycle per kick. Reaching `DRIVER_OK` therefore exercises
//! the whole 3c wiring: MMIO routing, the split-virtqueue build, and the
//! IRQ path (no buffers complete yet, so no interrupt is required to get
//! this far).
//!
//! Skips (and passes) without the kernel + initramfs fixtures. Needs a
//! kernel built with `CONFIG_VIRTIO_VSOCKETS` (the current
//! tinyconfig.fragment) — rebuild via `tools/kernel/build-tiny-kernel.sh`
//! after pulling this branch.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p vm-kvm --features kvm --test vsock_driver_ok_boot -- --nocapture
//! ```

#![cfg(feature = "kvm")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vm_core::{Hypervisor, VmConfig};
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

#[test]
fn guest_vsock_driver_reaches_driver_ok() {
    let Some(kernel) = resolve("NANOVM_TEST_KERNEL", "tools/kernel/cache/bzImage") else {
        eprintln!("vsock_driver_ok_boot: skipping — run tools/kernel/build-tiny-kernel.sh first.");
        return;
    };
    let Some(initrd) = resolve(
        "NANOVM_TEST_INITRAMFS",
        "tools/initramfs/cache/initramfs.cpio",
    ) else {
        eprintln!("vsock_driver_ok_boot: skipping — run tools/initramfs/build-initramfs.sh first.");
        return;
    };
    eprintln!(
        "vsock_driver_ok_boot: kernel={} initrd={}",
        kernel.display(),
        initrd.display(),
    );

    let hv = KvmHypervisor::new().expect("open /dev/kvm");
    let cfg = VmConfig {
        vcpus: 1,
        memory_mib: 128,
        kernel: Some(kernel),
        initrd: Some(initrd),
        vsock_cid: Some(3),
        cmdline: "console=ttyS0,115200 panic=-1 rdinit=/init".into(),
        ..VmConfig::default()
    };

    let handle = hv.create_vm(&cfg).expect("create_vm");
    hv.start(handle.id).expect("start");

    // The vsock driver binds during kernel init and sets DRIVER_OK once
    // its queues are up. Poll until it does; 30 s is generous.
    let deadline = Instant::now() + Duration::from_secs(30);
    let driver_ok = loop {
        let ok = hv
            .vsock_driver_ok(handle.id)
            .expect("vsock_driver_ok")
            .expect("vsock device should exist when vsock_cid is set");
        if ok || Instant::now() >= deadline {
            break ok;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let status = hv.vsock_status(handle.id).ok().flatten().unwrap_or(0);
    let serial = hv
        .serial_output(handle.id)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let _ = hv.stop(handle.id);
    let _ = hv.destroy(handle.id);

    assert!(
        driver_ok,
        "guest vsock driver never reached DRIVER_OK (status={status:#x}); is the kernel \
         built with CONFIG_VIRTIO_VSOCKETS?\n  serial:\n{serial}",
    );
    eprintln!("vsock_driver_ok_boot: DRIVER_OK reached (status={status:#x})");
}
