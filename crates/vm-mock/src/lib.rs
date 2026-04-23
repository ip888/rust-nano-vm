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
use std::sync::Mutex;

use vm_core::{Hypervisor, SnapshotId, VmConfig, VmError, VmHandle, VmId, VmResult, VmState};

#[derive(Debug, Clone)]
struct MockVm {
    config: VmConfig,
    state: VmState,
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
        let id = VmId::next();
        let vm = MockVm {
            config: cfg.clone(),
            state: VmState::Created,
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
            },
        );
        Ok(VmHandle { id, state })
    }

    fn destroy(&self, id: VmId) -> VmResult<()> {
        let mut inner = self.inner.lock().expect("mock hypervisor poisoned");
        if inner.vms.remove(&id).is_none() {
            return Err(VmError::UnknownVm(id));
        }
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
}
