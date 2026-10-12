//! Sub-PR #D commit 6/6 integration test: prove the guest **discovers
//! and drives** vm-kvm's virtio-net device when the host side is
//! wired to the smoltcp userspace TCP/IP backend.
//!
//! Mirrors `virtio_net_boot.rs` (which uses the mock backend) but
//! flips [`VmConfig::net_backend`] to [`NetBackendChoice::Smoltcp`].
//! The guest kernel's `virtio_net` driver is identical in both cases
//! — the point is to prove the KVM-side wiring works end-to-end:
//!
//! 1. vm-kvm sees `Smoltcp` and builds a [`virtio_net::SmoltcpBackend`]
//!    behind the virtio-net device model (instead of a `MockBackend`).
//! 2. The guest completes the virtio bring-up (status reaches
//!    `DRIVER_OK`), exposing `eth0` to userspace just like with the
//!    mock backend.
//! 3. [`KvmHypervisor::net_smoltcp_backend`] hands the caller an
//!    `Arc<SmoltcpBackend>` live on the running VM, ready to be
//!    wrapped in a [`virtio_net::TcpProxy`] for host-side byte
//!    forwarding.
//!
//! Skips (and passes) without the kernel + initramfs fixtures.
//! Needs a kernel built with `CONFIG_VIRTIO_NET` +
//! `CONFIG_VIRTIO_MMIO[_CMDLINE_DEVICES]`. Rebuild via
//! `tools/kernel/build-tiny-kernel.sh` if your cached bzImage is
//! older than sub-PR #C commit 4.
//!
//! # TODO (end-to-end data-path test — follow-up PR)
//!
//! This test only verifies the virtio bring-up handshake reaches
//! `DRIVER_OK` with the smoltcp backend wired in. It does **not**
//! configure `eth0` inside the guest, run a guest TCP listener,
//! bridge a host stream through [`TcpProxy`], or assert a real
//! HTTP round trip. That needs a guest initramfs with a small TCP
//! server (minimal Rust binary or a Spring Boot container) baked
//! into fixtures. Tracked as Copilot finding #4252252446 on
//! PR #283 for the follow-up PR / Milestone 1.5.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p vm-kvm --features kvm,virtio-net-smoltcp \
//!     --test virtio_net_smoltcp_boot -- --nocapture
//! ```

#![cfg(all(feature = "kvm", feature = "virtio-net-smoltcp"))]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vm_core::{Hypervisor, NetBackendChoice, VmConfig};
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
fn guest_drives_virtio_net_with_smoltcp_backend() {
    let Some(kernel) = resolve("NANOVM_TEST_KERNEL", "tools/kernel/cache/bzImage") else {
        eprintln!(
            "virtio_net_smoltcp_boot: skipping — run tools/kernel/build-tiny-kernel.sh first."
        );
        return;
    };
    let Some(initrd) = resolve(
        "NANOVM_TEST_INITRAMFS",
        "tools/initramfs/cache/initramfs.cpio",
    ) else {
        eprintln!(
            "virtio_net_smoltcp_boot: skipping — run tools/initramfs/build-initramfs.sh first."
        );
        return;
    };
    eprintln!(
        "virtio_net_smoltcp_boot: kernel={} initrd={}",
        kernel.display(),
        initrd.display(),
    );

    let hv = KvmHypervisor::new().expect("open /dev/kvm");
    let cfg = VmConfig {
        vcpus: 1,
        memory_mib: 128,
        kernel: Some(kernel),
        initrd: Some(initrd),
        // The one bit this test is really about: swap the mock
        // backend out for the real userspace TCP/IP stack.
        net_backend: NetBackendChoice::Smoltcp,
        cmdline: "console=ttyS0,115200 panic=-1 rdinit=/init".into(),
        ..VmConfig::default()
    };

    let handle = hv.create_vm(&cfg).expect("create_vm");
    hv.start(handle.id).expect("start");

    // The smoltcp backend handle must be live on the VM right after
    // creation — the proxy wiring in commit #5 relies on this.
    // Grab it before polling the virtio bring-up so a slow kernel
    // doesn't push this assertion past its real cause.
    let smoltcp = hv
        .net_smoltcp_backend(handle.id)
        .expect("net_smoltcp_backend lookup")
        .expect("smoltcp backend must be attached when NetBackendChoice::Smoltcp was selected");
    eprintln!(
        "virtio_net_smoltcp_boot: SmoltcpBackend listening on :{}",
        smoltcp.listen_port()
    );

    // Poll the same signals as `virtio_net_boot.rs` — the guest
    // kernel doesn't know (or care) which host-side backend is
    // wired behind the device, so the bring-up sequence is byte-
    // identical.
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
        "guest did not ACKNOWLEDGE the virtio-net device with smoltcp \
         backend (status={status:#x}); check that `virtio_mmio.device=` \
         is appended for net and that smoltcp wiring left MMIO reads \
         intact.\n  serial:\n{serial}",
    );
    assert!(
        driver_ok,
        "guest did not reach DRIVER_OK on the virtio-net device with \
         smoltcp backend (status={status:#x}); feature negotiation or \
         queue setup likely regressed in the smoltcp dispatch path.\n  \
         serial:\n{serial}",
    );
    eprintln!(
        "virtio_net_smoltcp_boot: status={status:#x} driver_ok={driver_ok} \
         (smoltcp-backed virtio-net bring-up succeeded)"
    );
}
