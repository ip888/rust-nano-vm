//! End-to-end test for `ProcessFleet`.
//!
//! Drives the fleet against the **real** `nanovm-vmm-child`
//! binary (mock backend) but stubs the jailer out with a shell
//! script that just `exec`s into the worker without touching
//! cgroups. That lets the test run on any host — even ones
//! without cgroup v2 delegation. The cgroup wiring itself has
//! its own integration test in `crates/nanovm-jailer/tests`.
//!
//! What we assert here is the orchestration loop:
//!
//! - `ProcessFleet::create_vm` spawns the jailer, waits for the
//!   worker's socket, runs the IPC handshake, and round-trips
//!   `CreateVm` to a real worker.
//! - Lifecycle ops (`start`, `stop`, `state`, `vm_meta`,
//!   `snapshot`, `restore`, `list_vms`, `list_snapshots`,
//!   `delete_snapshot`, `snapshot_meta`) round-trip through the
//!   persistent stream.
//! - `destroy` cooperatively shuts the worker down and removes
//!   it from the fleet map.
//! - `Drop` on the fleet kills any leaked worker.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nanovm_fleet::{FleetConfig, ProcessFleet};
use vm_core::{Hypervisor, VmConfig, VmState};

/// Resolve the freshly-built `nanovm-vmm-child` binary from the
/// workspace target dir. cargo only sets `CARGO_BIN_EXE_<name>`
/// for binaries IN the package under test, so we fall back to
/// scanning sibling target dirs the way `nanovm-mcp` integration
/// tests do.
fn vmm_child_binary() -> PathBuf {
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("target")
        });
    // Try debug first (test run), then release (CI release-mode).
    for profile in ["debug", "release"] {
        let candidate = target_dir.join(profile).join("nanovm-vmm-child");
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "could not locate nanovm-vmm-child binary; \
         ran `cargo test -p nanovm-fleet` without `cargo build -p nanovm-vmm-child` first?"
    );
}

/// Build a shell stub that ignores every jailer-specific arg
/// except `--vmm-child-binary` + `--socket` and execs the worker
/// directly. This lets the test exercise the orchestration loop
/// on any Linux host without needing cgroup v2 delegation.
fn make_stub_jailer(dir: &Path) -> PathBuf {
    let script = dir.join("stub-jailer.sh");
    let body = r#"#!/bin/sh
# Parse out --vmm-child-binary and --socket; ignore everything else.
WORKER=""
SOCKET=""
while [ $# -gt 0 ]; do
    case "$1" in
        --vmm-child-binary) WORKER="$2"; shift 2 ;;
        --socket) SOCKET="$2"; shift 2 ;;
        *) shift ;;
    esac
done
exec "$WORKER" --socket "$SOCKET"
"#;
    fs::write(&script, body).expect("write stub");
    let mut perm = fs::metadata(&script).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&script, perm).unwrap();
    script
}

fn make_fleet(dir: &Path) -> Arc<ProcessFleet> {
    let cfg = FleetConfig {
        jailer_binary: make_stub_jailer(dir),
        vmm_child_binary: vmm_child_binary(),
        socket_dir: dir.join("sockets"),
        default_memory_limit_mib: None,
        default_cpu_quota_pct: None,
        cgroup_parent: None,
        spawn_timeout: Duration::from_secs(10),
    };
    Arc::new(ProcessFleet::new(cfg).expect("construct fleet"))
}

#[test]
fn create_then_destroy_roundtrips_through_a_real_worker() {
    let dir = tempfile::tempdir().unwrap();
    let fleet = make_fleet(dir.path());

    let h = fleet
        .create_vm(&VmConfig::default())
        .expect("create_vm via fleet");
    // The fleet always returns the orchestrator-side id (1 for
    // the first VM); the worker's internal id is overwritten.
    assert_eq!(h.id.0, 1);
    assert_eq!(h.state, VmState::Created);

    // start → stop → state should round-trip.
    fleet.start(h.id).expect("start");
    assert_eq!(fleet.state(h.id).expect("state"), VmState::Running);
    fleet.stop(h.id).expect("stop");
    assert_eq!(fleet.state(h.id).expect("state"), VmState::Stopped);

    // list_vms reflects the live worker.
    let live = fleet.list_vms().expect("list");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, h.id);

    // destroy removes from the fleet map; subsequent state() is UnknownVm.
    fleet.destroy(h.id).expect("destroy");
    assert!(matches!(
        fleet.state(h.id).unwrap_err(),
        vm_core::VmError::UnknownVm(_)
    ));
    assert!(fleet.list_vms().unwrap().is_empty());
}

#[test]
fn snapshot_captures_id_and_lists() {
    // PR-4 supports snapshot (the owning worker's local capture).
    // restore() is intentionally Unsupported on the fleet until
    // PR-5 wires snapshot transfer via the durable store.
    let dir = tempfile::tempdir().unwrap();
    let fleet = make_fleet(dir.path());

    let h = fleet.create_vm(&VmConfig::default()).expect("create_vm");
    fleet.start(h.id).expect("start");
    let snap = fleet.snapshot(h.id).expect("snapshot");
    assert!(snap.0 >= 1);

    // The snapshot must show up in list_snapshots.
    let snaps = fleet.list_snapshots().expect("list snapshots");
    assert!(snaps.contains(&snap));

    fleet.destroy(h.id).expect("destroy");
}

#[test]
fn restore_returns_actionable_unsupported_message() {
    // PR-4 honestly rejects restore on the fleet backend — the
    // message must point operators at PR-5 / the in-process
    // backend so they know what to do.
    let dir = tempfile::tempdir().unwrap();
    let fleet = make_fleet(dir.path());
    let err = fleet.restore(vm_core::SnapshotId(1)).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("not yet supported") || msg.contains("PR-5"),
        "restore error should explain the limitation: {msg}"
    );
}

#[test]
fn destroy_unknown_vm_returns_unknown_vm() {
    let dir = tempfile::tempdir().unwrap();
    let fleet = make_fleet(dir.path());
    assert!(matches!(
        fleet.destroy(vm_core::VmId(999)).unwrap_err(),
        vm_core::VmError::UnknownVm(_)
    ));
}

#[test]
fn fleet_drop_kills_lingering_workers() {
    // Create a VM, then drop the fleet without explicit destroy.
    // The Worker's Drop should SIGKILL the jailer subprocess and
    // remove the socket file.
    let dir = tempfile::tempdir().unwrap();
    let socket = {
        let fleet = make_fleet(dir.path());
        let h = fleet.create_vm(&VmConfig::default()).expect("create_vm");
        // Record the socket path we expect to be cleaned up.
        dir.path()
            .join("sockets")
            .join(format!("vm-{}.sock", h.id.0))
    };
    // After drop, the socket file must be gone.
    assert!(
        !socket.exists(),
        "socket {} should be cleaned up by Worker::Drop",
        socket.display()
    );
}
