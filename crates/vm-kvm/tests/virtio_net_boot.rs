//! Sub-PR #C commit 5/5 integration test: prove the guest **discovers
//! and drives** vm-kvm's virtio-net device over the virtio-MMIO
//! transport.
//!
//! The KVM backend attaches an `MmioTransport` at `NET_MMIO_BASE`
//! (sub-PR #C commits 1-4) and appends
//! `virtio_mmio.device=0x1000@0xd0001000:6` to the guest kernel
//! cmdline. During early boot the kernel's `virtio_mmio` driver
//! parses that directive, probes the Magic/Version/DeviceID
//! registers, recognises a virtio device, and the virtio core:
//!
//! 1. Writes `ACKNOWLEDGE` into the status register.
//! 2. Writes `DRIVER` once a driver (`virtio_net`) is bound.
//! 3. Reads the device-feature bits, writes back the subset it
//!    accepts, then writes `FEATURES_OK`.
//! 4. Programs the RX/TX descriptor tables + ring addresses, writes
//!    `QueueReady = 1` per queue.
//! 5. Writes `DRIVER_OK` — the data path is now live and the guest
//!    exposes `eth0` to userspace.
//!
//! We observe step (5) host-side via `KvmHypervisor::net_driver_ok`.
//! A `true` return proves the guest ran the full virtio-net
//! bring-up: register probe + feature negotiation + queue setup.
//!
//! Skips (and passes) without the kernel + initramfs fixtures.
//! Needs a kernel built with `CONFIG_VIRTIO_NET` +
//! `CONFIG_VIRTIO_MMIO[_CMDLINE_DEVICES]` (the current
//! `tinyconfig.fragment` has these). Rebuild via
//! `tools/kernel/build-tiny-kernel.sh` after pulling this branch
//! if your cached bzImage was built without the net driver.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p vm-kvm --features kvm --test virtio_net_boot -- --nocapture
//! ```

#![cfg(feature = "kvm")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vm_core::{Hypervisor, VmConfig};
use vm_kvm::KvmHypervisor;

/// `STATUS_ACKNOWLEDGE` from the virtio spec — the first bit the
/// guest sets once it recognises the device.
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
fn guest_probes_virtio_net_device() {
    let Some(kernel) = resolve("NANOVM_TEST_KERNEL", "tools/kernel/cache/bzImage") else {
        eprintln!("virtio_net_boot: skipping — run tools/kernel/build-tiny-kernel.sh first.");
        return;
    };
    let Some(initrd) = resolve(
        "NANOVM_TEST_INITRAMFS",
        "tools/initramfs/cache/initramfs.cpio",
    ) else {
        eprintln!("virtio_net_boot: skipping — run tools/initramfs/build-initramfs.sh first.");
        return;
    };
    eprintln!(
        "virtio_net_boot: kernel={} initrd={}",
        kernel.display(),
        initrd.display(),
    );

    let hv = KvmHypervisor::new().expect("open /dev/kvm");
    let cfg = VmConfig {
        vcpus: 1,
        memory_mib: 128,
        kernel: Some(kernel),
        initrd: Some(initrd),
        // No vsock_cid here — pure net test. The net device is
        // attached unconditionally by vm-kvm (sub-PR #C commit 3).
        cmdline: "console=ttyS0,115200 panic=-1 rdinit=/init".into(),
        ..VmConfig::default()
    };

    let handle = hv.create_vm(&cfg).expect("create_vm");
    hv.start(handle.id).expect("start");

    // Poll the net device status. Two signals:
    //   - `net_status` ≥ ACKNOWLEDGE proves the virtio-mmio core
    //     parsed our cmdline and probed Magic/Version/DeviceID.
    //   - `net_driver_ok` returning `true` proves the kernel's
    //     `virtio_net` driver got through the whole bring-up
    //     (features + queues).
    //
    // 30 s matches the vsock_probe_boot timeout; a cold tinyconfig
    // kernel + minimal initramfs typically ACKNOWLEDGEs in <2 s and
    // reaches DRIVER_OK in <5 s under KVM.
    let deadline = Instant::now() + Duration::from_secs(30);
    let (status, driver_ok) = loop {
        let s = hv
            .net_status(handle.id)
            .expect("net_status")
            .expect("net device should always exist in a KVM-backed VM");
        let drv = hv
            .net_driver_ok(handle.id)
            .expect("net_driver_ok")
            .expect("net device should always exist in a KVM-backed VM");
        if drv || Instant::now() >= deadline {
            break (s, drv);
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
        "guest did not ACKNOWLEDGE the virtio-net device \
         (status={status:#x}); the virtio_mmio driver never probed it.\n  serial:\n{serial}",
    );
    assert!(
        driver_ok,
        "guest did not reach DRIVER_OK on the virtio-net device \
         (status={status:#x}); kernel probably lacks CONFIG_VIRTIO_NET \
         or the virtio_net driver failed feature negotiation.\n  serial:\n{serial}",
    );
    eprintln!(
        "virtio_net_boot: status={status:#x} driver_ok={driver_ok} \
         (full virtio-net bring-up succeeded)"
    );
}
