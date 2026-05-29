//! M2/D3 capstone integration test: a full `exec_in_guest` round-trip
//! over virtio-vsock.
//!
//! This exercises the entire host↔guest data path end to end:
//!
//! 1. The guest boots the real `nanovm-agent` as PID 1. Seeing
//!    `NANOVM_AGENT_VSOCK` in its environment (the host appends it to
//!    the kernel cmdline when `vsock_cid` is set), the agent connects
//!    out over AF_VSOCK to `(HOST_CID, 1024)`.
//! 2. The host accepts the connection in its `VsockDevice`.
//! 3. `exec_in_guest` frames an `Exec` request, the device writes it
//!    into the guest's rx virtqueue and raises the IRQ, the guest
//!    kernel delivers it to the agent's socket.
//! 4. The agent runs the program and writes a framed `ExecResult` back
//!    over the tx virtqueue; the host reassembles and decodes it.
//!
//! The program we run is `/init` — the agent binary itself, the one
//! executable guaranteed to exist in the minimal initramfs. The agent
//! strips its own transport env vars from children, so the spawned
//! `/init` falls back to stdio mode, reads EOF on its null stdin, and
//! exits 0. A clean `exit_code == Some(0)` therefore proves the request
//! reached the agent, ran, and the response came back — the whole loop.
//!
//! Skips (and passes) when the fixtures aren't built:
//!   tools/kernel/build-tiny-kernel.sh
//!   NANOVM_INIT=agent tools/initramfs/build-initramfs.sh
//!
//! Run with:
//!
//! ```sh
//! cargo test -p vm-kvm --features kvm --test exec_roundtrip_boot -- --nocapture
//! ```

#![cfg(feature = "kvm")]

use std::path::PathBuf;

use vm_core::{GuestExecRequest, Hypervisor, VmConfig};
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
fn exec_in_guest_round_trips_over_vsock() {
    let Some(kernel) = resolve("NANOVM_TEST_KERNEL", "tools/kernel/cache/bzImage") else {
        eprintln!("exec_roundtrip_boot: skipping — run tools/kernel/build-tiny-kernel.sh first.");
        return;
    };
    let Some(initrd) = resolve(
        "NANOVM_TEST_AGENT_INITRAMFS",
        "tools/initramfs/cache/initramfs-agent.cpio",
    ) else {
        eprintln!(
            "exec_roundtrip_boot: skipping — run \
             `NANOVM_INIT=agent tools/initramfs/build-initramfs.sh` first."
        );
        return;
    };
    eprintln!(
        "exec_roundtrip_boot: kernel={} initrd={}",
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

    // Run the agent binary itself as a child; transport env vars are
    // stripped from children, so it exits 0 after reading EOF on stdin.
    let result = hv.exec_in_guest(
        handle.id,
        GuestExecRequest {
            program: "/init".into(),
            args: vec![],
            cwd: None,
            env: vec![],
            timeout_ms: Some(10_000),
        },
    );

    let serial = hv
        .serial_output(handle.id)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let _ = hv.stop(handle.id);
    let _ = hv.destroy(handle.id);

    let exec = result.unwrap_or_else(|e| {
        panic!("exec_in_guest failed: {e}\n  serial:\n{serial}");
    });
    eprintln!(
        "exec_roundtrip_boot: exit_code={:?} signal={:?} stdout={}B stderr={}B duration_ms={}",
        exec.exit_code,
        exec.signal,
        exec.stdout.len(),
        exec.stderr.len(),
        exec.duration_ms,
    );
    assert_eq!(
        exec.exit_code,
        Some(0),
        "expected the child /init to exit 0 (proving the full vsock exec round-trip)\n  \
         stderr:\n{}\n  serial:\n{serial}",
        String::from_utf8_lossy(&exec.stderr),
    );
}
