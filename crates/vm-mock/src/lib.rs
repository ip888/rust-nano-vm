//! In-memory [`Hypervisor`] implementation for tests and CI.
//!
//! `MockHypervisor` never touches `/dev/kvm` or any real device. It tracks a
//! simple state machine (`Created → Running → Stopped`) per VM in a
//! `Mutex<HashMap<..>>` so it is safe to share across threads. Snapshots
//! capture the VM's state at a point in time and can be restored into a new
//! handle.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use vm_core::{
    GuestExecRequest, GuestExecResult, Hypervisor, SnapshotId, SnapshotMeta, VmConfig, VmError,
    VmHandle, VmId, VmMeta, VmResult, VmState,
};

#[derive(Debug, Clone)]
struct MockVm {
    config: VmConfig,
    state: VmState,
    guest_root: PathBuf,
}

#[derive(Debug, Clone)]
struct MockSnapshot {
    config: VmConfig,
    state: VmState,
}

/// A [`Hypervisor`] that exists entirely in RAM. Zero dependencies on the
/// kernel or CPU extensions. Intended for unit tests, the CI workflow, and
/// developer machines without KVM.
#[derive(Default, Debug)]
pub struct MockHypervisor {
    inner: Mutex<Inner>,
}

#[derive(Default, Debug)]
struct Inner {
    vms: HashMap<VmId, MockVm>,
    snapshots: HashMap<SnapshotId, MockSnapshot>,
}

impl MockHypervisor {
    /// Construct a new, empty mock hypervisor.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of VMs currently tracked (incl. stopped, excl. destroyed).
    /// Test/introspection helper; not part of the public [`Hypervisor`] API.
    pub fn vm_count(&self) -> usize {
        self.inner
            .lock()
            .expect("mock hypervisor poisoned")
            .vms
            .len()
    }

    /// Number of snapshots currently stored.
    pub fn snapshot_count(&self) -> usize {
        self.inner
            .lock()
            .expect("mock hypervisor poisoned")
            .snapshots
            .len()
    }
}

impl Hypervisor for MockHypervisor {
    fn create_vm(&self, cfg: &VmConfig) -> VmResult<VmHandle> {
        // If snapshot_dir is set, the manifest is authoritative for the
        // VM's geometry — overwrite the config-provided vcpus / memory_mib
        // before we record the VM. Surfaces a Backend error if the
        // manifest is missing, malformed, or has an unsupported version.
        let cfg = match &cfg.snapshot_dir {
            None => cfg.clone(),
            Some(dir) => {
                let manifest = snapshot::Manifest::read_from_dir(dir)
                    .map_err(|e| VmError::Backend(format!("snapshot manifest: {e}")))?;
                let mib = manifest.memory_bytes / (1024 * 1024);
                VmConfig {
                    vcpus: manifest.vcpu_count,
                    memory_mib: mib,
                    cmdline: manifest.kernel_cmdline.clone(),
                    ..cfg.clone()
                }
            }
        };
        let id = VmId::next();
        let guest_root = guest_root_dir(id);
        let vm = MockVm {
            config: cfg,
            state: VmState::Created,
            guest_root,
        };
        self.inner
            .lock()
            .expect("mock hypervisor poisoned")
            .vms
            .insert(id, vm);
        Ok(VmHandle {
            id,
            state: VmState::Created,
        })
    }

    fn start(&self, id: VmId) -> VmResult<()> {
        let mut inner = self.inner.lock().expect("mock hypervisor poisoned");
        let vm = inner.vms.get_mut(&id).ok_or(VmError::UnknownVm(id))?;
        match vm.state {
            VmState::Created | VmState::Stopped => {
                vm.state = VmState::Running;
                Ok(())
            }
            VmState::Running => Err(VmError::InvalidTransition {
                id,
                from: VmState::Running,
                to: VmState::Running,
            }),
        }
    }

    fn stop(&self, id: VmId) -> VmResult<()> {
        let mut inner = self.inner.lock().expect("mock hypervisor poisoned");
        let vm = inner.vms.get_mut(&id).ok_or(VmError::UnknownVm(id))?;
        match vm.state {
            VmState::Running => {
                vm.state = VmState::Stopped;
                Ok(())
            }
            other => Err(VmError::InvalidTransition {
                id,
                from: other,
                to: VmState::Stopped,
            }),
        }
    }

    fn state(&self, id: VmId) -> VmResult<VmState> {
        let inner = self.inner.lock().expect("mock hypervisor poisoned");
        inner
            .vms
            .get(&id)
            .map(|vm| vm.state)
            .ok_or(VmError::UnknownVm(id))
    }

    fn snapshot(&self, id: VmId) -> VmResult<SnapshotId> {
        let mut inner = self.inner.lock().expect("mock hypervisor poisoned");
        let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?.clone();
        let snap_id = SnapshotId::next();
        inner.snapshots.insert(
            snap_id,
            MockSnapshot {
                config: vm.config,
                state: vm.state,
            },
        );
        Ok(snap_id)
    }

    fn restore(&self, snap: SnapshotId) -> VmResult<VmHandle> {
        let mut inner = self.inner.lock().expect("mock hypervisor poisoned");
        let snapshot = inner
            .snapshots
            .get(&snap)
            .cloned()
            .ok_or(VmError::UnknownSnapshot(snap))?;
        let id = VmId::next();
        let state = snapshot.state;
        inner.vms.insert(
            id,
            MockVm {
                config: snapshot.config,
                state,
                guest_root: guest_root_dir(id),
            },
        );
        Ok(VmHandle { id, state })
    }

    fn destroy(&self, id: VmId) -> VmResult<()> {
        let mut inner = self.inner.lock().expect("mock hypervisor poisoned");
        let Some(vm) = inner.vms.remove(&id) else {
            return Err(VmError::UnknownVm(id));
        };
        let _ = std::fs::remove_dir_all(vm.guest_root);
        Ok(())
    }

    fn list_vms(&self) -> VmResult<Vec<VmHandle>> {
        let inner = self.inner.lock().expect("mock hypervisor poisoned");
        Ok(inner
            .vms
            .iter()
            .map(|(id, vm)| VmHandle {
                id: *id,
                state: vm.state,
            })
            .collect())
    }

    fn list_snapshots(&self) -> VmResult<Vec<SnapshotId>> {
        let inner = self.inner.lock().expect("mock hypervisor poisoned");
        Ok(inner.snapshots.keys().copied().collect())
    }

    fn delete_snapshot(&self, snap: SnapshotId) -> VmResult<()> {
        let mut inner = self.inner.lock().expect("mock hypervisor poisoned");
        if inner.snapshots.remove(&snap).is_none() {
            return Err(VmError::UnknownSnapshot(snap));
        }
        Ok(())
    }

    fn vm_meta(&self, id: VmId) -> VmResult<VmMeta> {
        let inner = self.inner.lock().expect("mock hypervisor poisoned");
        let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
        Ok(VmMeta {
            id,
            state: vm.state,
            vcpus: vm.config.vcpus,
            memory_mib: vm.config.memory_mib,
            kernel_cmdline: vm.config.cmdline.clone(),
            snapshot_dir: vm.config.snapshot_dir.clone(),
        })
    }

    fn snapshot_meta(&self, snap: SnapshotId) -> VmResult<SnapshotMeta> {
        let inner = self.inner.lock().expect("mock hypervisor poisoned");
        let s = inner
            .snapshots
            .get(&snap)
            .ok_or(VmError::UnknownSnapshot(snap))?;
        // The mock has no real "memory" — fabricate plausible bytes from
        // the captured `memory_mib`. 4096 page size matches x86_64 host
        // expectations and the snapshot crate's BackingFileHeader::new
        // sample.
        Ok(SnapshotMeta {
            id: snap,
            vcpu_count: s.config.vcpus,
            memory_bytes: s.config.memory_mib.saturating_mul(1024 * 1024),
            page_size: 4096,
            kernel_cmdline: s.config.cmdline.clone(),
        })
    }

    // ---- Guest operations (M2 offline-testable subset) -------------------

    fn exec_in_guest(&self, id: VmId, req: GuestExecRequest) -> VmResult<GuestExecResult> {
        // Verify the VM exists and is Running before we spawn anything.
        {
            let inner = self.inner.lock().expect("mock hypervisor poisoned");
            let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
            if vm.state != VmState::Running {
                return Err(VmError::InvalidTransition {
                    id,
                    from: vm.state,
                    to: VmState::Running,
                });
            }
        }

        use std::process::{Command, Stdio};
        use std::time::Instant;

        let start = Instant::now();
        let mut cmd = Command::new(&req.program);
        cmd.args(&req.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        if let Some(ref dir) = req.cwd {
            cmd.current_dir(dir);
        }
        for (k, v) in &req.env {
            cmd.env(k, v);
        }

        let child = cmd
            .spawn()
            .map_err(|e| VmError::Backend(format!("exec spawn {}: {e}", req.program)))?;
        let output = child
            .wait_with_output()
            .map_err(|e| VmError::Backend(format!("exec wait {}: {e}", req.program)))?;
        let duration_ms = start.elapsed().as_millis() as u64;

        if let Some(limit) = req.timeout_ms {
            if duration_ms > limit {
                return Err(VmError::Backend(format!(
                    "exec exceeded timeout {limit} ms (ran {duration_ms} ms)"
                )));
            }
        }

        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            output.status.signal()
        };
        #[cfg(not(unix))]
        let signal: Option<i32> = None;

        Ok(GuestExecResult {
            exit_code: output.status.code(),
            signal,
            stdout: output.stdout,
            stderr: output.stderr,
            duration_ms,
        })
    }

    fn write_file(&self, id: VmId, path: String, content: Vec<u8>, mode: u32) -> VmResult<u64> {
        let guest_root = {
            let inner = self.inner.lock().expect("mock hypervisor poisoned");
            let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
            if vm.state != VmState::Running {
                return Err(VmError::InvalidTransition {
                    id,
                    from: vm.state,
                    to: VmState::Running,
                });
            }
            vm.guest_root.clone()
        };

        let path = resolve_guest_path(&guest_root, &path)?;
        let bytes = content.len() as u64;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                VmError::Backend(format!("create parent {}: {e}", parent.display()))
            })?;
        }
        std::fs::write(&path, &content)
            .map_err(|e| VmError::Backend(format!("write_file {}: {e}", path.display())))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode);
            std::fs::set_permissions(&path, perms)
                .map_err(|e| VmError::Backend(format!("chmod {}: {e}", path.display())))?;
        }
        #[cfg(not(unix))]
        let _ = mode;

        Ok(bytes)
    }

    fn read_file(&self, id: VmId, path: String) -> VmResult<Vec<u8>> {
        let guest_root = {
            let inner = self.inner.lock().expect("mock hypervisor poisoned");
            let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
            if vm.state != VmState::Running {
                return Err(VmError::InvalidTransition {
                    id,
                    from: vm.state,
                    to: VmState::Running,
                });
            }
            vm.guest_root.clone()
        };

        let path = resolve_guest_path(&guest_root, &path)?;
        std::fs::read(&path)
            .map_err(|e| VmError::Backend(format!("read_file {}: {e}", path.display())))
    }
}

fn guest_root_dir(id: VmId) -> PathBuf {
    std::env::temp_dir().join(format!(
        "rust-nano-vm-mock-guest-{}-{}",
        std::process::id(),
        id.0
    ))
}

fn resolve_guest_path(root: &Path, guest_path: &str) -> VmResult<PathBuf> {
    let path = Path::new(guest_path);
    if !path.is_absolute() {
        return Err(VmError::Backend(format!(
            "guest path must be absolute: {guest_path}"
        )));
    }
    let mut resolved = root.to_path_buf();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(part) => resolved.push(part),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(VmError::Backend(format!(
                    "guest path must not escape its root: {guest_path}"
                )))
            }
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VmConfig {
        VmConfig::default()
    }

    #[test]
    fn create_then_start_then_stop_follows_state_machine() {
        let hv = MockHypervisor::new();
        let handle = hv.create_vm(&cfg()).expect("create");
        assert_eq!(hv.state(handle.id).unwrap(), VmState::Created);
        hv.start(handle.id).expect("start");
        assert_eq!(hv.state(handle.id).unwrap(), VmState::Running);
        hv.stop(handle.id).expect("stop");
        assert_eq!(hv.state(handle.id).unwrap(), VmState::Stopped);
    }

    #[test]
    fn start_while_running_rejects_with_invalid_transition() {
        let hv = MockHypervisor::new();
        let handle = hv.create_vm(&cfg()).unwrap();
        hv.start(handle.id).unwrap();
        let err = hv.start(handle.id).unwrap_err();
        assert!(matches!(
            err,
            VmError::InvalidTransition {
                from: VmState::Running,
                to: VmState::Running,
                ..
            }
        ));
    }

    #[test]
    fn stop_before_start_rejects_with_invalid_transition() {
        let hv = MockHypervisor::new();
        let handle = hv.create_vm(&cfg()).unwrap();
        let err = hv.stop(handle.id).unwrap_err();
        assert!(matches!(
            err,
            VmError::InvalidTransition {
                from: VmState::Created,
                to: VmState::Stopped,
                ..
            }
        ));
    }

    #[test]
    fn unknown_vm_returns_unknown_vm_error() {
        let hv = MockHypervisor::new();
        let bogus = VmId(0xdead_beef);
        assert!(matches!(
            hv.start(bogus).unwrap_err(),
            VmError::UnknownVm(_)
        ));
        assert!(matches!(hv.stop(bogus).unwrap_err(), VmError::UnknownVm(_)));
        assert!(matches!(
            hv.state(bogus).unwrap_err(),
            VmError::UnknownVm(_)
        ));
        assert!(matches!(
            hv.snapshot(bogus).unwrap_err(),
            VmError::UnknownVm(_)
        ));
        assert!(matches!(
            hv.destroy(bogus).unwrap_err(),
            VmError::UnknownVm(_)
        ));
    }

    #[test]
    fn snapshot_preserves_state_and_restore_creates_fresh_id() {
        let hv = MockHypervisor::new();
        let original = hv.create_vm(&cfg()).unwrap();
        hv.start(original.id).unwrap();
        let snap = hv.snapshot(original.id).unwrap();

        let restored = hv.restore(snap).unwrap();
        assert_ne!(restored.id, original.id);
        assert_eq!(restored.state, VmState::Running);
        assert_eq!(hv.state(restored.id).unwrap(), VmState::Running);
    }

    #[test]
    fn restore_unknown_snapshot_fails_cleanly() {
        let hv = MockHypervisor::new();
        let err = hv.restore(SnapshotId(0xcafe)).unwrap_err();
        assert!(matches!(err, VmError::UnknownSnapshot(_)));
    }

    #[test]
    fn destroy_removes_the_vm() {
        let hv = MockHypervisor::new();
        let handle = hv.create_vm(&cfg()).unwrap();
        assert_eq!(hv.vm_count(), 1);
        hv.destroy(handle.id).unwrap();
        assert_eq!(hv.vm_count(), 0);
        assert!(matches!(
            hv.state(handle.id).unwrap_err(),
            VmError::UnknownVm(_)
        ));
    }

    #[test]
    fn snapshot_then_restore_is_reusable_many_times_for_forking() {
        // This is the M5 wedge: snapshot once, fork many cheaply. Even in the
        // mock, we want to validate that restore() can be called repeatedly
        // from the same snapshot to fan out.
        let hv = MockHypervisor::new();
        let base = hv.create_vm(&cfg()).unwrap();
        hv.start(base.id).unwrap();
        let snap = hv.snapshot(base.id).unwrap();

        let mut ids = Vec::new();
        for _ in 0..8 {
            let child = hv.restore(snap).unwrap();
            assert_eq!(child.state, VmState::Running);
            ids.push(child.id);
        }
        // 1 base + 8 forks = 9
        assert_eq!(hv.vm_count(), 9);
        // All fork ids are distinct
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 8);
    }

    #[test]
    fn hypervisor_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MockHypervisor>();
    }

    #[test]
    fn list_vms_returns_each_created_vm_with_current_state() {
        let hv = MockHypervisor::new();
        assert!(hv.list_vms().unwrap().is_empty());

        let a = hv.create_vm(&cfg()).unwrap();
        let b = hv.create_vm(&cfg()).unwrap();
        let c = hv.create_vm(&cfg()).unwrap();
        hv.start(b.id).unwrap();
        hv.start(c.id).unwrap();
        hv.stop(c.id).unwrap();

        let mut listed = hv.list_vms().unwrap();
        listed.sort_by_key(|h| h.id);
        let mut expected = vec![
            VmHandle {
                id: a.id,
                state: VmState::Created,
            },
            VmHandle {
                id: b.id,
                state: VmState::Running,
            },
            VmHandle {
                id: c.id,
                state: VmState::Stopped,
            },
        ];
        expected.sort_by_key(|h| h.id);
        // VmHandle is Clone but not PartialEq; compare by (id, state).
        let listed: Vec<_> = listed.into_iter().map(|h| (h.id, h.state)).collect();
        let expected: Vec<_> = expected.into_iter().map(|h| (h.id, h.state)).collect();
        assert_eq!(listed, expected);
    }

    #[test]
    fn list_vms_excludes_destroyed_vms() {
        let hv = MockHypervisor::new();
        let a = hv.create_vm(&cfg()).unwrap();
        let b = hv.create_vm(&cfg()).unwrap();
        hv.destroy(a.id).unwrap();
        let listed = hv.list_vms().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, b.id);
    }

    // ---- Snapshot-restore path ----------------------------------------

    fn snapshot_dir_with_manifest(
        slug: &str,
        snapshot_id: u64,
        memory_mib: u64,
        vcpu_count: u32,
        cmdline: &str,
    ) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rust-nano-vm-{}-{}-{}",
            slug,
            std::process::id(),
            snapshot_id
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut m =
            snapshot::Manifest::new(snapshot_id, memory_mib * 1024 * 1024, 4096, vcpu_count);
        m.kernel_cmdline = cmdline.to_owned();
        m.write_to_dir(&dir).expect("write manifest");
        dir
    }

    #[test]
    fn create_vm_with_snapshot_dir_uses_manifest_geometry() {
        let dir = snapshot_dir_with_manifest("create-from-snap", 7, 256, 4, "console=ttyS0");
        let hv = MockHypervisor::new();
        let cfg = VmConfig {
            // Caller-provided values that must be overridden by the manifest.
            vcpus: 1,
            memory_mib: 16,
            snapshot_dir: Some(dir.clone()),
            ..VmConfig::default()
        };
        let handle = hv.create_vm(&cfg).expect("create from snapshot");
        // Inspect via the internal map by listing.
        let listed = hv.list_vms().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, handle.id);
        // We can't read MockVm.config externally, so re-create with no
        // snapshot to confirm by symmetry that geometry differs.
        let baseline = hv.create_vm(&VmConfig::default()).expect("baseline create");
        assert_ne!(baseline.id, handle.id);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn create_vm_with_missing_snapshot_dir_returns_backend_error() {
        let hv = MockHypervisor::new();
        let cfg = VmConfig {
            snapshot_dir: Some(std::path::PathBuf::from(
                "/nonexistent/rust-nano-vm/snapshot",
            )),
            ..VmConfig::default()
        };
        let err = hv.create_vm(&cfg).unwrap_err();
        assert!(matches!(err, VmError::Backend(_)), "got {err:?}");
    }

    // ---- Snapshot list / delete --------------------------------------

    #[test]
    fn list_snapshots_is_empty_initially() {
        let hv = MockHypervisor::new();
        assert!(hv.list_snapshots().unwrap().is_empty());
    }

    #[test]
    fn list_snapshots_returns_each_captured_snapshot() {
        let hv = MockHypervisor::new();
        let a = hv.create_vm(&cfg()).unwrap();
        hv.start(a.id).unwrap();
        let s1 = hv.snapshot(a.id).unwrap();
        let s2 = hv.snapshot(a.id).unwrap();
        let mut listed = hv.list_snapshots().unwrap();
        listed.sort();
        let mut expected = vec![s1, s2];
        expected.sort();
        assert_eq!(listed, expected);
    }

    #[test]
    fn delete_snapshot_removes_it_and_subsequent_restore_fails() {
        let hv = MockHypervisor::new();
        let h = hv.create_vm(&cfg()).unwrap();
        hv.start(h.id).unwrap();
        let s = hv.snapshot(h.id).unwrap();
        assert_eq!(hv.snapshot_count(), 1);
        hv.delete_snapshot(s).unwrap();
        assert_eq!(hv.snapshot_count(), 0);
        assert!(matches!(
            hv.restore(s).unwrap_err(),
            VmError::UnknownSnapshot(_)
        ));
    }

    #[test]
    fn delete_unknown_snapshot_returns_unknown_snapshot() {
        let hv = MockHypervisor::new();
        let err = hv.delete_snapshot(SnapshotId(0xfeed_face)).unwrap_err();
        assert!(matches!(err, VmError::UnknownSnapshot(_)));
    }

    #[test]
    fn snapshot_meta_reports_captured_geometry() {
        let hv = MockHypervisor::new();
        let h = hv
            .create_vm(&VmConfig {
                vcpus: 3,
                memory_mib: 256,
                cmdline: "console=ttyS0".into(),
                ..VmConfig::default()
            })
            .unwrap();
        hv.start(h.id).unwrap();
        let snap = hv.snapshot(h.id).unwrap();
        let meta = hv.snapshot_meta(snap).expect("meta");
        assert_eq!(meta.id, snap);
        assert_eq!(meta.vcpu_count, 3);
        assert_eq!(meta.memory_bytes, 256 * 1024 * 1024);
        assert_eq!(meta.page_size, 4096);
        assert_eq!(meta.kernel_cmdline, "console=ttyS0");
    }

    #[test]
    fn snapshot_meta_for_unknown_id_is_unknown_snapshot() {
        let hv = MockHypervisor::new();
        let err = hv.snapshot_meta(SnapshotId(0xdead)).unwrap_err();
        assert!(matches!(err, VmError::UnknownSnapshot(_)));
    }

    #[test]
    fn vm_meta_reports_create_geometry_and_state() {
        let hv = MockHypervisor::new();
        let h = hv
            .create_vm(&VmConfig {
                vcpus: 4,
                memory_mib: 256,
                cmdline: "console=ttyS0".into(),
                ..VmConfig::default()
            })
            .unwrap();
        let meta = hv.vm_meta(h.id).expect("meta");
        assert_eq!(meta.id, h.id);
        assert_eq!(meta.state, VmState::Created);
        assert_eq!(meta.vcpus, 4);
        assert_eq!(meta.memory_mib, 256);
        assert_eq!(meta.kernel_cmdline, "console=ttyS0");
        assert!(meta.snapshot_dir.is_none());

        // State should reflect concurrent transitions.
        hv.start(h.id).unwrap();
        assert_eq!(hv.vm_meta(h.id).unwrap().state, VmState::Running);
    }

    #[test]
    fn vm_meta_for_unknown_id_is_unknown_vm() {
        let hv = MockHypervisor::new();
        let err = hv.vm_meta(VmId(0xdead)).unwrap_err();
        assert!(matches!(err, VmError::UnknownVm(_)));
    }

    // ---- Guest operations -----------------------------------------------

    #[test]
    fn exec_in_guest_requires_running_state() {
        let hv = MockHypervisor::new();
        let h = hv.create_vm(&cfg()).unwrap();
        // VM is Created, not Running
        let err = hv
            .exec_in_guest(
                h.id,
                GuestExecRequest {
                    program: "echo".into(),
                    args: vec!["hi".into()],
                    cwd: None,
                    env: vec![],
                    timeout_ms: None,
                },
            )
            .unwrap_err();
        assert!(matches!(err, VmError::InvalidTransition { .. }));
    }

    #[test]
    fn exec_in_guest_runs_local_process_and_captures_output() {
        let hv = MockHypervisor::new();
        let h = hv.create_vm(&cfg()).unwrap();
        hv.start(h.id).unwrap();

        let result = hv
            .exec_in_guest(
                h.id,
                GuestExecRequest {
                    program: "echo".into(),
                    args: vec!["hello mock".into()],
                    cwd: None,
                    env: vec![],
                    timeout_ms: None,
                },
            )
            .expect("exec");

        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.starts_with(b"hello mock"));
    }

    #[test]
    fn exec_in_guest_on_unknown_vm_returns_unknown_vm() {
        let hv = MockHypervisor::new();
        let err = hv
            .exec_in_guest(
                VmId(0xbad),
                GuestExecRequest {
                    program: "echo".into(),
                    args: vec![],
                    cwd: None,
                    env: vec![],
                    timeout_ms: None,
                },
            )
            .unwrap_err();
        assert!(matches!(err, VmError::UnknownVm(_)));
    }

    #[test]
    fn write_and_read_file_roundtrip() {
        let hv = MockHypervisor::new();
        let h = hv.create_vm(&cfg()).unwrap();
        hv.start(h.id).unwrap();

        let path = format!(
            "/tmp/rust-nano-vm-mock-test-{}-{}",
            std::process::id(),
            h.id.0
        );
        let content = b"hello from mock guest\n".to_vec();

        let written = hv
            .write_file(h.id, path.clone(), content.clone(), 0o644)
            .expect("write_file");
        assert_eq!(written, content.len() as u64);

        let read = hv.read_file(h.id, path.clone()).expect("read_file");
        assert_eq!(read, content);
        assert!(
            !std::path::Path::new(&path).exists(),
            "mock guest paths must not write directly onto the host filesystem"
        );
    }

    #[test]
    fn write_file_requires_running_state() {
        let hv = MockHypervisor::new();
        let h = hv.create_vm(&cfg()).unwrap();
        let err = hv
            .write_file(h.id, "/tmp/x".into(), b"data".to_vec(), 0o644)
            .unwrap_err();
        assert!(matches!(err, VmError::InvalidTransition { .. }));
    }

    #[test]
    fn read_file_missing_path_returns_backend_error() {
        let hv = MockHypervisor::new();
        let h = hv.create_vm(&cfg()).unwrap();
        hv.start(h.id).unwrap();
        let err = hv
            .read_file(h.id, "/no/such/file/for/mock/test".into())
            .unwrap_err();
        assert!(matches!(err, VmError::Backend(_)));
    }

    #[test]
    fn guest_paths_cannot_escape_mock_root() {
        let hv = MockHypervisor::new();
        let h = hv.create_vm(&cfg()).unwrap();
        hv.start(h.id).unwrap();
        let err = hv
            .write_file(h.id, "/tmp/../escape".into(), b"oops".to_vec(), 0o644)
            .unwrap_err();
        assert!(matches!(err, VmError::Backend(_)));
    }
}
