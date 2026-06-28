//! Per-org ownership tracking for VMs and snapshots.
//!
//! The control plane assigns every newly created VM and snapshot to
//! the calling [`OrgId`] and rejects subsequent operations from a
//! different org with a 403 `cross_org`. List endpoints transparently
//! filter to the caller's org.
//!
//! ## Why here, not in `vm-core`
//!
//! `vm-core::Hypervisor` is intentionally backend-agnostic — it
//! doesn't know about tokens or orgs. Ownership is a *control-plane*
//! concept (it's about who's allowed to call the API, not about the
//! VM itself), so we keep it strictly above the trait boundary.
//!
//! ## Resilience on restart
//!
//! Ownership is currently in-memory: when the control plane restarts,
//! every VM/snapshot the hypervisor still knows about is treated as
//! belonging to the [`OrgId::default_org()`]. That's fine for the
//! mock backend (which loses its VMs on restart anyway) and for the
//! single-tenant default-org case; multi-tenant production must
//! persist this map. A future PR adds a SQLite backing.
//!
//! ## What happens when a VM/snapshot has no recorded owner
//!
//! We treat unrecorded resources as owned by the default org. This is
//! the legacy-compat fall-through: deployments existing before this
//! module rolled out keep working, single-tenant setups don't break,
//! and multi-tenant setups never see unrecorded resources at runtime
//! because every successful create_vm / restore / fork / snapshot
//! records ownership.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use vm_core::{SnapshotId, VmId};

use crate::auth::OrgId;
use crate::error::ApiError;

/// In-memory ownership maps. Cheap to construct; shared via `Arc` from
/// [`crate::AppState`].
#[derive(Debug, Default)]
pub struct OwnershipMap {
    vms: Mutex<HashMap<VmId, OrgId>>,
    snapshots: Mutex<HashMap<SnapshotId, OrgId>>,
}

impl OwnershipMap {
    /// Record `org` as the owner of `vm`. Overwrites any prior
    /// recorded owner (the previous owner is discarded; no merge).
    pub fn record_vm(&self, vm: VmId, org: OrgId) {
        let mut guard = self.vms.lock().expect("vm ownership map poisoned");
        guard.insert(vm, org);
    }

    /// Record `org` as the owner of `snap`.
    pub fn record_snapshot(&self, snap: SnapshotId, org: OrgId) {
        let mut guard = self
            .snapshots
            .lock()
            .expect("snapshot ownership map poisoned");
        guard.insert(snap, org);
    }

    /// Drop the recorded owner of `vm`. Called on `destroy`. Idempotent.
    pub fn forget_vm(&self, vm: VmId) {
        let mut guard = self.vms.lock().expect("vm ownership map poisoned");
        guard.remove(&vm);
    }

    /// Drop the recorded owner of `snap`. Called on `delete_snapshot`.
    pub fn forget_snapshot(&self, snap: SnapshotId) {
        let mut guard = self
            .snapshots
            .lock()
            .expect("snapshot ownership map poisoned");
        guard.remove(&snap);
    }

    /// Look up the recorded owner of `vm`, falling back to
    /// [`OrgId::default_org()`] for legacy / unrecorded resources.
    pub fn vm_owner(&self, vm: VmId) -> OrgId {
        let guard = self.vms.lock().expect("vm ownership map poisoned");
        guard.get(&vm).cloned().unwrap_or_else(OrgId::default_org)
    }

    /// Look up the recorded owner of `snap`.
    pub fn snapshot_owner(&self, snap: SnapshotId) -> OrgId {
        let guard = self
            .snapshots
            .lock()
            .expect("snapshot ownership map poisoned");
        guard.get(&snap).cloned().unwrap_or_else(OrgId::default_org)
    }

    /// Verify `caller` may touch `vm`. Errors with
    /// [`ApiError::Forbidden`] (`cross_org`) when the recorded owner
    /// disagrees.
    pub fn require_vm_access(&self, vm: VmId, caller: &OrgId) -> Result<(), ApiError> {
        let owner = self.vm_owner(vm);
        if &owner == caller {
            Ok(())
        } else {
            Err(ApiError::Forbidden {
                code: "cross_org",
                message: format!(
                    "vm {} is owned by a different org; \
                     refusing cross-org access",
                    vm.0
                ),
            })
        }
    }

    /// Verify `caller` may touch `snap`.
    pub fn require_snapshot_access(
        &self,
        snap: SnapshotId,
        caller: &OrgId,
    ) -> Result<(), ApiError> {
        let owner = self.snapshot_owner(snap);
        if &owner == caller {
            Ok(())
        } else {
            Err(ApiError::Forbidden {
                code: "cross_org",
                message: format!(
                    "snapshot {} is owned by a different org; \
                     refusing cross-org access",
                    snap.0
                ),
            })
        }
    }

    /// Return the set of VM ids owned by `caller`. Used by `list_vms`
    /// to filter the hypervisor's full inventory down to the caller's
    /// scope. Resources without a recorded owner are treated as
    /// belonging to the default org.
    ///
    /// Currently unused by the handlers (they iterate inline against
    /// [`vm_owner`](Self::vm_owner) so the default-org fall-through
    /// works correctly); kept for the per-org metering PR-A2 which
    /// needs to enumerate every owned resource per billing tick.
    #[allow(dead_code)]
    pub fn vms_owned_by(&self, caller: &OrgId) -> HashSet<VmId> {
        let guard = self.vms.lock().expect("vm ownership map poisoned");
        guard
            .iter()
            .filter(|(_, org)| *org == caller)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Return the set of snapshot ids owned by `caller`. Same caveat as
    /// [`vms_owned_by`](Self::vms_owned_by): unused at the handler
    /// layer today; needed by the per-org metering PR-A2.
    #[allow(dead_code)]
    pub fn snapshots_owned_by(&self, caller: &OrgId) -> HashSet<SnapshotId> {
        let guard = self
            .snapshots
            .lock()
            .expect("snapshot ownership map poisoned");
        guard
            .iter()
            .filter(|(_, org)| *org == caller)
            .map(|(id, _)| *id)
            .collect()
    }

    /// `true` when the caller is the default org. Helper for code that
    /// wants to widen list/filter logic for the legacy single-tenant
    /// case. Unused by the handlers today; kept for the operator UI
    /// work in a later PR.
    #[allow(dead_code)]
    pub fn caller_is_default(caller: &OrgId) -> bool {
        caller == &OrgId::default_org()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn org(s: &str) -> OrgId {
        OrgId::new(s)
    }

    #[test]
    fn record_then_require_passes_for_same_org() {
        let m = OwnershipMap::default();
        m.record_vm(VmId(1), org("acme"));
        assert!(m.require_vm_access(VmId(1), &org("acme")).is_ok());
    }

    #[test]
    fn require_rejects_cross_org_with_forbidden() {
        let m = OwnershipMap::default();
        m.record_vm(VmId(1), org("acme"));
        let err = m.require_vm_access(VmId(1), &org("globex")).unwrap_err();
        match err {
            ApiError::Forbidden { code, .. } => assert_eq!(code, "cross_org"),
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }

    #[test]
    fn unrecorded_vm_belongs_to_default_org() {
        let m = OwnershipMap::default();
        // Nobody recorded VmId(7). Default org is allowed.
        assert!(m.require_vm_access(VmId(7), &OrgId::default_org()).is_ok());
        // A non-default org sees a Forbidden.
        let err = m.require_vm_access(VmId(7), &org("acme")).unwrap_err();
        assert!(matches!(err, ApiError::Forbidden { .. }));
    }

    #[test]
    fn forget_drops_the_recorded_owner() {
        let m = OwnershipMap::default();
        m.record_vm(VmId(1), org("acme"));
        m.forget_vm(VmId(1));
        // Falls back to default.
        assert_eq!(m.vm_owner(VmId(1)), OrgId::default_org());
    }

    #[test]
    fn record_overwrites_previous_owner() {
        let m = OwnershipMap::default();
        m.record_vm(VmId(1), org("acme"));
        m.record_vm(VmId(1), org("globex"));
        assert_eq!(m.vm_owner(VmId(1)), org("globex"));
    }

    #[test]
    fn snapshots_independent_from_vms() {
        let m = OwnershipMap::default();
        m.record_vm(VmId(1), org("acme"));
        m.record_snapshot(SnapshotId(1), org("globex"));
        assert!(m.require_vm_access(VmId(1), &org("acme")).is_ok());
        assert!(m
            .require_snapshot_access(SnapshotId(1), &org("globex"))
            .is_ok());
        assert!(m
            .require_snapshot_access(SnapshotId(1), &org("acme"))
            .is_err());
    }

    #[test]
    fn vms_owned_by_returns_only_callers_resources() {
        let m = OwnershipMap::default();
        m.record_vm(VmId(1), org("acme"));
        m.record_vm(VmId(2), org("acme"));
        m.record_vm(VmId(3), org("globex"));
        let acme: Vec<_> = {
            let mut v: Vec<_> = m.vms_owned_by(&org("acme")).into_iter().collect();
            v.sort_by_key(|id| id.0);
            v
        };
        assert_eq!(acme, vec![VmId(1), VmId(2)]);
        let globex: Vec<_> = m.vms_owned_by(&org("globex")).into_iter().collect();
        assert_eq!(globex, vec![VmId(3)]);
    }

    #[test]
    fn caller_is_default_recognizes_default_org() {
        assert!(OwnershipMap::caller_is_default(&OrgId::default_org()));
        assert!(!OwnershipMap::caller_is_default(&org("acme")));
    }
}
