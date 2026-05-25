//! M2/D2 integration test: boot the guest with the **real Rust
//! guest-agent** as PID 1 and prove it launches inside the VM.
//!
//! Builds on D1 (initramfs / guest userspace). Where the D1 fixture
//! was a ~15-line C init, this boots the actual `nanovm-agent`
//! static-musl binary as `/init`. The agent prints a readiness
//! banner to stderr on startup; with `console=ttyS0` that reaches
//! the host's serial capture, proving the agent runs as guest
//! userspace — the last piece before wiring a host↔guest transport
//! (D3).
//!
//! The agent then blocks reading requests from its stdin (the
//! console) — there's no transport yet — so the guest stays
//! `Running`. This test therefore polls the serial output for the
//! banner and succeeds as soon as it appears, rather than waiting
//! for a terminal state.
//!
//! Skips (and passes) when the fixtures aren't built:
//!   tools/kernel/build-tiny-kernel.sh
//!   NANOVM_INIT=agent tools/initramfs/build-initramfs.sh
//!
//! Run with:
//!
//! ```sh
//! cargo test -p vm-kvm --features kvm agent_init_boot -- --nocapture
//! ```

#![cfg(feature = "kvm")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vm_core::{Hypervisor, VmConfig};
use vm_kvm::KvmHypervisor;

/// Substring of the agent's startup banner
/// (`nanovm-agent: ready (proto v1, ...)`).
const AGENT_BANNER: &str = "nanovm-agent: ready";

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

/// Poll the guest's serial output until it contains `needle` or the
/// deadline passes. Returns the full captured output. Used instead
/// of waiting for a terminal state because the agent blocks serving
/// requests and keeps the guest Running.
fn wait_for_serial(
    hv: &KvmHypervisor,
    id: vm_core::VmId,
    needle: &str,
    timeout: Duration,
) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let out = hv.serial_output(id).expect("serial_output");
        let s = String::from_utf8_lossy(&out).into_owned();
        if s.contains(needle) || Instant::now() >= deadline {
            return s;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn agent_launches_as_guest_init() {
    let Some(kernel) = resolve("NANOVM_TEST_KERNEL", "tools/kernel/cache/bzImage") else {
        eprintln!("agent_init_boot: skipping — run tools/kernel/build-tiny-kernel.sh first.");
        return;
    };
    let Some(initrd) = resolve(
        "NANOVM_TEST_AGENT_INITRAMFS",
        "tools/initramfs/cache/initramfs-agent.cpio",
    ) else {
        eprintln!(
            "agent_init_boot: skipping — run \
             `NANOVM_INIT=agent tools/initramfs/build-initramfs.sh` first."
        );
        return;
    };
    eprintln!(
        "agent_init_boot: kernel={} initrd={}",
        kernel.display(),
        initrd.display(),
    );

    let hv = KvmHypervisor::new().expect("open /dev/kvm");
    let cfg = VmConfig {
        vcpus: 1,
        memory_mib: 128,
        kernel: Some(kernel),
        initrd: Some(initrd),
        cmdline: "console=ttyS0,115200 panic=-1 rdinit=/init".into(),
        ..VmConfig::default()
    };

    let handle = hv.create_vm(&cfg).expect("create_vm");
    hv.start(handle.id).expect("start");

    let serial = wait_for_serial(&hv, handle.id, AGENT_BANNER, Duration::from_secs(30));
    let run_err = hv.last_run_error(handle.id).ok().flatten();

    // Stop the still-running guest before asserting so the VM is
    // always cleaned up even on failure.
    let _ = hv.stop(handle.id);
    let _ = hv.destroy(handle.id);

    assert!(
        serial.contains(AGENT_BANNER),
        "agent banner {AGENT_BANNER:?} not found in serial output\n  \
         last_run_error={run_err:?}\n  serial:\n{serial}",
    );
}
