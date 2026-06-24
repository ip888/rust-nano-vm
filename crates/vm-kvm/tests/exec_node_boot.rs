//! JavaScript counterpart of `exec_python_boot`: run
//! `node -e "console.log(1+1)"` inside a guest microVM and assert
//! stdout is `"2\n"`. Same shape as the python test, different
//! language toolchain — proves the platform isn't language-locked.
//!
//! - Kernel: the same tinyconfig bzImage as the other vm-kvm boot
//!   tests, built by `tools/kernel/build-tiny-kernel.sh`.
//! - Initramfs: an Alpine 3.20 rootfs with `nodejs` baked in and
//!   `nanovm-agent` as `/init`, produced by
//!   `tools/node-rootfs/build.sh`. The kernel decompresses it on
//!   load (CONFIG_RD_GZIP=y in the tinyconfig fragment).
//!
//! What this exercises:
//!
//! 1. Guest boots, agent connects out over AF_VSOCK to (HOST_CID, 1024).
//! 2. Host frames a `proto::Request::Exec { program: "node",
//!    args: ["-e", "console.log(1+1)"], ... }`, the agent reads it.
//! 3. Agent spawns `node`, captures stdout, returns
//!    `proto::Response::ExecResult { stdout: b"2\n", exit_code: 0, ... }`.
//! 4. Host decodes, asserts exit_code = 0 and stdout = "2\n".
//!
//! Skips (and passes) when the fixtures aren't built:
//!   tools/kernel/build-tiny-kernel.sh
//!   tools/node-rootfs/build.sh
//!
//! Memory note: Alpine + Node 20 wants more headroom than the
//! minimal initramfs tests use. 256 MiB is empirically comfortable;
//! 128 MiB risks OOM on first-call V8 warmup.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p vm-kvm --features kvm --test exec_node_boot -- --nocapture
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
fn node_console_log_one_plus_one_runs_inside_guest() {
    let Some(kernel) = resolve("NANOVM_TEST_KERNEL", "tools/kernel/cache/bzImage") else {
        eprintln!("exec_node_boot: skipping — run tools/kernel/build-tiny-kernel.sh first.");
        return;
    };
    let Some(initrd) = resolve(
        "NANOVM_TEST_NODE_INITRAMFS",
        "tools/node-rootfs/cache/initramfs-node.cpio.gz",
    ) else {
        eprintln!(
            "exec_node_boot: skipping — run \
             `tools/node-rootfs/build.sh` first to produce the node initramfs."
        );
        return;
    };
    eprintln!(
        "exec_node_boot: kernel={} initrd={}",
        kernel.display(),
        initrd.display(),
    );

    let hv = KvmHypervisor::new().expect("open /dev/kvm");
    let cfg = VmConfig {
        vcpus: 1,
        // 256 MiB so V8's initial-heap setup doesn't get squeezed.
        // 128 MiB has been observed to OOM during Node startup on
        // some Alpine builds.
        memory_mib: 256,
        kernel: Some(kernel),
        initrd: Some(initrd),
        vsock_cid: Some(3),
        cmdline: "console=ttyS0,115200 panic=-1 rdinit=/init".into(),
        ..VmConfig::default()
    };

    let handle = hv.create_vm(&cfg).expect("create_vm");
    hv.start(handle.id).expect("start");

    let result = hv.exec_in_guest(
        handle.id,
        GuestExecRequest {
            program: "node".into(),
            args: vec!["-e".into(), "console.log(1+1)".into()],
            cwd: None,
            env: vec![],
            // Node first-invocation wall-time is dominated by V8's
            // self-init + JIT warmup — typically ~1-2 s on a stock
            // i5; allow 15 s of headroom in case the host is busy.
            timeout_ms: Some(15_000),
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
    let stdout = String::from_utf8_lossy(&exec.stdout);
    let stderr = String::from_utf8_lossy(&exec.stderr);
    eprintln!(
        "exec_node_boot: exit_code={:?} signal={:?} stdout={:?} stderr={:?} duration_ms={}",
        exec.exit_code, exec.signal, stdout, stderr, exec.duration_ms,
    );
    assert_eq!(
        exec.exit_code,
        Some(0),
        "expected node to exit 0\n  stderr:\n{stderr}\n  serial:\n{serial}",
    );
    assert_eq!(
        stdout, "2\n",
        "expected stdout = \"2\\n\" — proves node ran the program and the host got the bytes\n  \
         stderr:\n{stderr}\n  serial:\n{serial}",
    );
}
