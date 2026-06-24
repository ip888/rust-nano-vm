//! Integration test: streaming `exec_in_guest_stream` against the
//! real guest agent over virtio-vsock.
//!
//! Sister test to `exec_roundtrip_boot.rs`. Where that one proves the
//! one-shot `RequestBody::Exec` path, this one proves the streaming
//! `RequestBody::ExecStart` →  `ResponseBody::ExecOutput*` →
//! `ResponseBody::ExecExited` path through the KVM device.
//!
//! What this exercises:
//!
//! 1. Guest boots the agent as PID 1.
//! 2. Host calls `exec_in_guest_stream`, which sends `ExecStart` over
//!    vsock.
//! 3. Agent spawns `/init` (the agent binary; transport env vars
//!    are stripped from the child, so it falls back to stdio mode and
//!    exits 0 on EOF).
//! 4. Host iterates the returned [`ExecStream`] and collects every
//!    [`ExecFrame::Stdout`] / [`ExecFrame::Stderr`] chunk until the
//!    terminal [`ExecFrame::Exit`] arrives.
//! 5. We assert exit_code = 0 — proves the streaming wire path is
//!    intact end to end.
//!
//! Skips (and passes) when the fixtures aren't built:
//!   tools/kernel/build-tiny-kernel.sh
//!   NANOVM_INIT=agent tools/initramfs/build-initramfs.sh
//!
//! Run with:
//!
//! ```sh
//! cargo test -p vm-kvm --features kvm --test exec_stream_boot -- --nocapture
//! ```

#![cfg(feature = "kvm")]

use std::path::PathBuf;

use vm_core::{ExecFrame, GuestExecRequest, Hypervisor, VmConfig};
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
fn exec_in_guest_stream_round_trips_over_vsock() {
    let Some(kernel) = resolve("NANOVM_TEST_KERNEL", "tools/kernel/cache/bzImage") else {
        eprintln!("exec_stream_boot: skipping — run tools/kernel/build-tiny-kernel.sh first.");
        return;
    };
    let Some(initrd) = resolve(
        "NANOVM_TEST_AGENT_INITRAMFS",
        "tools/initramfs/cache/initramfs-agent.cpio",
    ) else {
        eprintln!(
            "exec_stream_boot: skipping — run \
             `NANOVM_INIT=agent tools/initramfs/build-initramfs.sh` first."
        );
        return;
    };
    eprintln!(
        "exec_stream_boot: kernel={} initrd={}",
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

    let stream_result = hv.exec_in_guest_stream(
        handle.id,
        GuestExecRequest {
            program: "/init".into(),
            args: vec![],
            cwd: None,
            env: vec![],
            timeout_ms: None,
        },
    );

    let mut stdout_bytes: Vec<u8> = Vec::new();
    let mut stderr_bytes: Vec<u8> = Vec::new();
    let mut exit_payload: Option<(Option<i32>, Option<i32>, u64)> = None;
    let collect_result = stream_result.and_then(|mut stream| {
        loop {
            match stream.next_frame()? {
                Some(ExecFrame::Stdout(bytes)) => stdout_bytes.extend_from_slice(&bytes),
                Some(ExecFrame::Stderr(bytes)) => stderr_bytes.extend_from_slice(&bytes),
                Some(ExecFrame::Exit {
                    exit_code,
                    signal,
                    duration_ms,
                }) => {
                    exit_payload = Some((exit_code, signal, duration_ms));
                    break;
                }
                None => break,
            }
        }
        Ok(())
    });

    let serial = hv
        .serial_output(handle.id)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let _ = hv.stop(handle.id);
    let _ = hv.destroy(handle.id);

    collect_result.unwrap_or_else(|e| {
        panic!("exec_in_guest_stream failed: {e}\n  serial:\n{serial}");
    });
    let (exit_code, signal, duration_ms) =
        exit_payload.expect("streaming exec never yielded a terminal Exit frame");
    eprintln!(
        "exec_stream_boot: exit_code={exit_code:?} signal={signal:?} stdout={}B stderr={}B duration_ms={duration_ms}",
        stdout_bytes.len(),
        stderr_bytes.len(),
    );
    assert_eq!(
        exit_code,
        Some(0),
        "expected the child /init to exit 0 (proving the full vsock exec_stream round-trip)\n  \
         stderr:\n{}\n  serial:\n{serial}",
        String::from_utf8_lossy(&stderr_bytes),
    );
}
