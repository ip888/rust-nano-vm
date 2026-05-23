//! M1 end-to-end integration test: boot a hand-rolled real-mode
//! program through the public `Hypervisor` trait, verify it ran
//! to completion, and assert what it wrote to the serial port.
//!
//! What this proves end-to-end on a real `/dev/kvm` host:
//!
//! - `KvmHypervisor::new()` opens `/dev/kvm`.
//! - `create_vm` with `VmConfig.flat_binary = Some(bytes)` builds a
//!   real-mode runtime that loads the bytes at GPA 0.
//! - `start` spawns the vCPU thread and lets it execute.
//! - The guest writes to COM1 (`0x3f8`) and the vCPU's `IoOut` exit
//!   reaches `handle_io_out`, which appends to `serial_output`.
//! - `Hlt` ends the vCPU thread cleanly.
//! - `state()` polls and observes the transition to `Stopped` once
//!   the thread has been reaped.
//! - `serial_output()` returns the bytes the guest emitted.
//!
//! `cfg(feature = "kvm")` gates everything — without the feature,
//! the file compiles to an empty test binary so
//! `cargo test --workspace` stays portable on machines without
//! `/dev/kvm`.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p vm-kvm --features kvm -- --test-threads=1 --nocapture
//! ```

#![cfg(feature = "kvm")]

use std::time::{Duration, Instant};

use vm_core::{Hypervisor, VmConfig, VmState};
use vm_kvm::KvmHypervisor;

/// Build the real-mode program:
///
/// ```asm
///   mov dx, 0x3f8         ; COM1 data port (16-bit immediate)
///   mov al, 'h'           ; per-character: load + out
///   out dx, al
///   …
///   hlt
/// ```
///
/// Byte sequence per char: `b0 XX ee` (load AL with imm8, then
/// `out dx, al`). The dx load is `ba f8 03`. The terminator is
/// `f4` (HLT).
fn hello_program() -> Vec<u8> {
    let mut prog = vec![0xba, 0xf8, 0x03]; // mov dx, 0x3f8
    for &b in b"hello\n" {
        prog.push(0xb0); // mov al, imm8
        prog.push(b);
        prog.push(0xee); // out dx, al
    }
    prog.push(0xf4); // hlt
    prog
}

/// Poll `hv.state(id)` every 10 ms up to `timeout` waiting for the
/// VM to leave `Running`. Returns the terminal state observed.
///
/// The vCPU thread updates state via `reap_finished_vcpus` on the
/// next `state` call after the thread exits, so polling is correct.
fn wait_for_terminal(hv: &KvmHypervisor, id: vm_core::VmId, timeout: Duration) -> VmState {
    let deadline = Instant::now() + timeout;
    loop {
        let st = hv.state(id).expect("query state");
        if st != VmState::Running {
            return st;
        }
        if Instant::now() >= deadline {
            return st;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn flat_binary_writes_hello_to_serial_and_halts() {
    let hv = KvmHypervisor::new().expect("open /dev/kvm");

    let cfg = VmConfig {
        vcpus: 1,
        memory_mib: 4,
        flat_binary: Some(hello_program()),
        // Defaults for everything else: kernel/rootfs unset
        // (mutually exclusive with flat_binary), no cmdline,
        // no snapshot, no vsock.
        ..VmConfig::default()
    };

    let handle = hv.create_vm(&cfg).expect("create_vm");
    assert_eq!(handle.state, VmState::Created);

    hv.start(handle.id).expect("start");
    // start() is non-blocking — wait for the vCPU thread to HLT.
    // Total program is 23 bytes of in-line I/O; should finish in
    // microseconds. 2 seconds is enormously generous and exists
    // only to surface a real bug (loop) rather than CI flake.
    let final_state = wait_for_terminal(&hv, handle.id, Duration::from_secs(2));
    assert_eq!(
        final_state,
        VmState::Stopped,
        "vCPU did not reach HLT within timeout — got {:?}",
        final_state,
    );

    let out = hv.serial_output(handle.id).expect("serial_output");
    assert_eq!(
        out,
        b"hello\n",
        "expected b\"hello\\n\", got {:?}",
        std::str::from_utf8(&out).unwrap_or("<non-utf8>"),
    );

    // Destroy so the next test starts from a clean slate.
    hv.destroy(handle.id).expect("destroy");
}

#[test]
fn flat_binary_and_kernel_path_are_mutually_exclusive() {
    let hv = KvmHypervisor::new().expect("open /dev/kvm");
    let cfg = VmConfig {
        flat_binary: Some(vec![0xf4]),                      // just HLT
        kernel: Some(std::path::PathBuf::from("/no/such")), // dummy
        ..VmConfig::default()
    };
    let err = hv.create_vm(&cfg).expect_err("must reject both set");
    assert!(
        format!("{err}").contains("mutually exclusive"),
        "expected mutual-exclusion error, got {err}",
    );
}

#[test]
fn flat_binary_too_large_for_memory_is_rejected() {
    let hv = KvmHypervisor::new().expect("open /dev/kvm");
    // 8 MiB binary into a 1 MiB guest → reject.
    let cfg = VmConfig {
        memory_mib: 1,
        flat_binary: Some(vec![0u8; 8 * 1024 * 1024]),
        ..VmConfig::default()
    };
    let err = hv.create_vm(&cfg).expect_err("must reject oversize");
    let msg = format!("{err}");
    assert!(
        msg.contains("exceeds guest memory") || msg.contains("flat_binary"),
        "expected size-related error, got {msg}",
    );
}
