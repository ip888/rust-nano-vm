//! Capstone integration test: `write_file` then `read_file` against a
//! real guest agent over virtio-vsock.
//!
//! This is the "cp into the sandbox" flow customers want: push a file
//! from the host into the guest filesystem, run something with it, and
//! pull a result back. The MockHypervisor already exercises this shape
//! against the host fs; this test proves it works against the real KVM
//! backend going through the same vsock RPC as `exec_in_guest`.
//!
//! What this exercises:
//!
//! 1. Guest boots the agent as PID 1; agent connects out to the host.
//! 2. Host frames a `proto::Request::WriteFile { path, content, mode }`,
//!    the device delivers it through the rx virtqueue.
//! 3. Agent writes the file to the guest's tmpfs and replies with
//!    `proto::Response::Written { bytes }`.
//! 4. Host frames a `proto::Request::ReadFile { path }`, the agent
//!    reads the file back and replies with `proto::Response::FileContent`.
//! 5. Host asserts the bytes match — proves the round-trip end to end.
//!
//! Skips (and passes) when the fixtures aren't built:
//!   tools/kernel/build-tiny-kernel.sh
//!   NANOVM_INIT=agent tools/initramfs/build-initramfs.sh
//!
//! Run with:
//!
//! ```sh
//! cargo test -p vm-kvm --features kvm --test file_roundtrip_boot -- --nocapture
//! ```

#![cfg(feature = "kvm")]

use std::path::PathBuf;

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
fn write_then_read_file_round_trips_over_vsock() {
    let Some(kernel) = resolve("NANOVM_TEST_KERNEL", "tools/kernel/cache/bzImage") else {
        eprintln!("file_roundtrip_boot: skipping — run tools/kernel/build-tiny-kernel.sh first.");
        return;
    };
    let Some(initrd) = resolve(
        "NANOVM_TEST_AGENT_INITRAMFS",
        "tools/initramfs/cache/initramfs-agent.cpio",
    ) else {
        eprintln!(
            "file_roundtrip_boot: skipping — run \
             `NANOVM_INIT=agent tools/initramfs/build-initramfs.sh` first."
        );
        return;
    };
    eprintln!(
        "file_roundtrip_boot: kernel={} initrd={}",
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

    // /tmp is the one path guaranteed writable by an unprivileged agent
    // in the minimal initramfs (it's a tmpfs mount, not the read-only
    // initramfs cpio).
    let path = "/tmp/nanovm-roundtrip.bin".to_owned();
    // Use a payload with non-printable bytes so we'd catch any
    // string-only round-trip bug — base64 of "hello" + raw NUL +
    // FF byte.
    let payload: Vec<u8> = (0u8..=255u8).collect();

    let write_result = hv.write_file(handle.id, path.clone(), payload.clone(), 0o644);
    let read_result = write_result.and_then(|n_written| {
        if (n_written as usize) != payload.len() {
            return Err(vm_core::VmError::Backend(format!(
                "write_file claimed {} bytes, expected {}",
                n_written,
                payload.len()
            )));
        }
        hv.read_file(handle.id, path.clone())
    });

    let serial = hv
        .serial_output(handle.id)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let _ = hv.stop(handle.id);
    let _ = hv.destroy(handle.id);

    let read_back = read_result.unwrap_or_else(|e| {
        panic!("file round-trip failed: {e}\n  serial:\n{serial}");
    });
    eprintln!(
        "file_roundtrip_boot: wrote {} bytes, read back {} bytes",
        payload.len(),
        read_back.len(),
    );
    assert_eq!(
        read_back, payload,
        "round-tripped bytes differ from what we wrote — \
         proves write_file/read_file made it through vsock RPC"
    );
}
