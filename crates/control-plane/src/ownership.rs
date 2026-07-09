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
//! ## Restart persistence
//!
//! The [`OwnershipMap`] struct is a thin façade over an
//! [`OwnershipStore`] trait object. Two backends ship:
//!
//! - [`InMemoryStore`] (default): a pair of `HashMap`s under `Mutex`.
//!   Every VM/snapshot the hypervisor still knows about after a restart
//!   falls back to [`OrgId::default_org()`], which is fine for mock and
//!   single-tenant deployments but fatal for multi-tenant SaaS.
//! - [`SqliteStore`] (feature-gated `sqlite`): writes each
//!   record/forget to a SQLite file; on startup the file is opened
//!   and the schema is migrated. Ownership survives control-plane
//!   restart, redeploy, and machine replacement (when the file lives
//!   on a mounted volume).
//!
//! Operators pick the backend at deploy time via the
//! `NANOVM_OWNERSHIP_STORE` env var:
//!
//! - unset (default) → `InMemoryStore`
//! - `sqlite:///data/nanovm.sqlite` or bare `/data/nanovm.sqlite`
//!   → `SqliteStore` (requires the binary to be built with `--features sqlite`).
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

/// Errors that can arise inside an [`OwnershipStore`] implementation
/// (SQLite disk full, DB locked, poisoned mutex, …). Kept at trait
/// scope so backends don't force callers to depend on rusqlite's or
/// std's error types.
#[derive(Debug, thiserror::Error)]
pub enum OwnershipStoreError {
    /// The backend failed to complete the operation. Message carries
    /// the backend-specific detail (SQLite error string, etc.).
    #[error("ownership store backend error: {0}")]
    Backend(String),
}

/// Storage backend for [`OwnershipMap`]. Read/write semantics:
/// `record_*` inserts or overwrites; `forget_*` is idempotent (removing
/// a non-existent id is not an error); lookups return `None` if the id
/// was never recorded — the calling [`OwnershipMap`] applies the
/// default-org fall-through on top.
pub trait OwnershipStore: Send + Sync + std::fmt::Debug {
    /// Insert or overwrite the recorded owner of `vm`.
    fn record_vm(&self, vm: VmId, org: &OrgId) -> Result<(), OwnershipStoreError>;
    /// Insert or overwrite the recorded owner of `snap`.
    fn record_snapshot(&self, snap: SnapshotId, org: &OrgId) -> Result<(), OwnershipStoreError>;
    /// Drop the recorded owner of `vm`. Idempotent.
    fn forget_vm(&self, vm: VmId) -> Result<(), OwnershipStoreError>;
    /// Drop the recorded owner of `snap`. Idempotent.
    fn forget_snapshot(&self, snap: SnapshotId) -> Result<(), OwnershipStoreError>;
    /// Look up the recorded owner of `vm`, or `None` if not recorded.
    fn vm_owner(&self, vm: VmId) -> Option<OrgId>;
    /// Look up the recorded owner of `snap`, or `None` if not recorded.
    fn snapshot_owner(&self, snap: SnapshotId) -> Option<OrgId>;
    /// Enumerate every VM id whose recorded owner is `org`.
    fn vms_owned_by(&self, org: &OrgId) -> HashSet<VmId>;
    /// Enumerate every snapshot id whose recorded owner is `org`.
    fn snapshots_owned_by(&self, org: &OrgId) -> HashSet<SnapshotId>;
}

/// In-memory ownership store. Cheap to construct; safe for single-tenant
/// deployments and for the mock backend (which loses its VMs on restart
/// anyway). Multi-tenant SaaS should use [`SqliteStore`] instead.
#[derive(Debug, Default)]
pub struct InMemoryStore {
    vms: Mutex<HashMap<VmId, OrgId>>,
    snapshots: Mutex<HashMap<SnapshotId, OrgId>>,
}

impl OwnershipStore for InMemoryStore {
    fn record_vm(&self, vm: VmId, org: &OrgId) -> Result<(), OwnershipStoreError> {
        let mut guard = self.vms.lock().expect("vm ownership map poisoned");
        guard.insert(vm, org.clone());
        Ok(())
    }
    fn record_snapshot(&self, snap: SnapshotId, org: &OrgId) -> Result<(), OwnershipStoreError> {
        let mut guard = self
            .snapshots
            .lock()
            .expect("snapshot ownership map poisoned");
        guard.insert(snap, org.clone());
        Ok(())
    }
    fn forget_vm(&self, vm: VmId) -> Result<(), OwnershipStoreError> {
        let mut guard = self.vms.lock().expect("vm ownership map poisoned");
        guard.remove(&vm);
        Ok(())
    }
    fn forget_snapshot(&self, snap: SnapshotId) -> Result<(), OwnershipStoreError> {
        let mut guard = self
            .snapshots
            .lock()
            .expect("snapshot ownership map poisoned");
        guard.remove(&snap);
        Ok(())
    }
    fn vm_owner(&self, vm: VmId) -> Option<OrgId> {
        let guard = self.vms.lock().expect("vm ownership map poisoned");
        guard.get(&vm).cloned()
    }
    fn snapshot_owner(&self, snap: SnapshotId) -> Option<OrgId> {
        let guard = self
            .snapshots
            .lock()
            .expect("snapshot ownership map poisoned");
        guard.get(&snap).cloned()
    }
    fn vms_owned_by(&self, org: &OrgId) -> HashSet<VmId> {
        let guard = self.vms.lock().expect("vm ownership map poisoned");
        guard
            .iter()
            .filter(|(_, o)| *o == org)
            .map(|(id, _)| *id)
            .collect()
    }
    fn snapshots_owned_by(&self, org: &OrgId) -> HashSet<SnapshotId> {
        let guard = self
            .snapshots
            .lock()
            .expect("snapshot ownership map poisoned");
        guard
            .iter()
            .filter(|(_, o)| *o == org)
            .map(|(id, _)| *id)
            .collect()
    }
}

// ---- SQLite backend (feature-gated) --------------------------------
#[cfg(feature = "sqlite")]
mod sqlite_backend;
#[cfg(feature = "sqlite")]
pub use sqlite_backend::SqliteStore;

// ---- Facade --------------------------------------------------------

/// Ownership facade shared via `Arc` from [`crate::AppState`]. Public
/// methods are infallible: a backend failure is logged with `warn!` and
/// the operation is treated as a best-effort success. This keeps the
/// call sites (route handlers) simple; the trade-off is that a
/// persistence hiccup can leave the in-memory view momentarily
/// inconsistent with the disk view until the next successful write.
#[derive(Debug)]
pub struct OwnershipMap {
    store: Box<dyn OwnershipStore>,
}

impl Default for OwnershipMap {
    fn default() -> Self {
        Self::new_in_memory()
    }
}

impl OwnershipMap {
    /// Wrap an [`InMemoryStore`]. Cheap; does not touch the filesystem.
    /// Same shape as the previous default constructor.
    pub fn new_in_memory() -> Self {
        Self {
            store: Box::new(InMemoryStore::default()),
        }
    }

    /// Wrap the given [`OwnershipStore`] implementation. Used by
    /// tests that want to plug in a custom store, and by the
    /// `NANOVM_OWNERSHIP_STORE` env-driven constructor.
    pub fn with_store(store: Box<dyn OwnershipStore>) -> Self {
        Self { store }
    }

    /// Pick a backend from the `NANOVM_OWNERSHIP_STORE` env var:
    ///
    /// - unset or empty → [`InMemoryStore`]
    /// - `sqlite:///path` or plain `/path` (with the `sqlite` feature)
    ///   → [`SqliteStore`] at that path
    ///
    /// Errors when a SQLite path is configured but the file can't be
    /// opened or migrated. Called once at startup in `server.rs`.
    pub fn from_env() -> Result<Self, OwnershipStoreError> {
        match std::env::var("NANOVM_OWNERSHIP_STORE") {
            // Unset → fall through to in-memory. Silent, matches the
            // documented behaviour.
            Err(std::env::VarError::NotPresent) => Ok(Self::new_in_memory()),
            // Present-but-non-UTF-8: refuse to boot. Silently
            // downgrading to in-memory here would mean a mangled Docker
            // env var reverts a prod deploy to non-persistent — the
            // caller expected persistence and there's no signal
            // otherwise. Better to fail startup loudly.
            Err(std::env::VarError::NotUnicode(bytes)) => Err(OwnershipStoreError::Backend(
                format!("NANOVM_OWNERSHIP_STORE is not valid UTF-8: {bytes:?}"),
            )),
            Ok(spec) if spec.is_empty() => Ok(Self::new_in_memory()),
            Ok(spec) => {
                let path = spec.strip_prefix("sqlite://").unwrap_or(&spec);
                #[cfg(feature = "sqlite")]
                {
                    tracing::info!(
                        path,
                        "ownership store: sqlite (persistent, multi-tenant safe)"
                    );
                    Ok(Self::with_store(Box::new(SqliteStore::open(path)?)))
                }
                #[cfg(not(feature = "sqlite"))]
                {
                    let _ = path;
                    Err(OwnershipStoreError::Backend(
                        "NANOVM_OWNERSHIP_STORE is set but this binary was built \
                         without the `sqlite` feature. Rebuild `control-plane` \
                         with `--features sqlite`, or unset the env var to fall \
                         back to the in-memory store."
                            .to_string(),
                    ))
                }
            }
        }
    }

    /// Record `org` as the owner of `vm`. Overwrites any prior owner.
    pub fn record_vm(&self, vm: VmId, org: OrgId) {
        if let Err(e) = self.store.record_vm(vm, &org) {
            tracing::warn!(vm = vm.0, error = %e, "ownership store: record_vm failed");
        }
    }

    /// Record `org` as the owner of `snap`.
    pub fn record_snapshot(&self, snap: SnapshotId, org: OrgId) {
        if let Err(e) = self.store.record_snapshot(snap, &org) {
            tracing::warn!(snapshot = snap.0, error = %e, "ownership store: record_snapshot failed");
        }
    }

    /// Drop the recorded owner of `vm`. Idempotent.
    pub fn forget_vm(&self, vm: VmId) {
        if let Err(e) = self.store.forget_vm(vm) {
            tracing::warn!(vm = vm.0, error = %e, "ownership store: forget_vm failed");
        }
    }

    /// Drop the recorded owner of `snap`.
    pub fn forget_snapshot(&self, snap: SnapshotId) {
        if let Err(e) = self.store.forget_snapshot(snap) {
            tracing::warn!(snapshot = snap.0, error = %e, "ownership store: forget_snapshot failed");
        }
    }

    /// Look up the recorded owner of `vm`, falling back to
    /// [`OrgId::default_org()`] for legacy / unrecorded resources.
    pub fn vm_owner(&self, vm: VmId) -> OrgId {
        self.store.vm_owner(vm).unwrap_or_else(OrgId::default_org)
    }

    /// Look up the recorded owner of `snap`.
    pub fn snapshot_owner(&self, snap: SnapshotId) -> OrgId {
        self.store
            .snapshot_owner(snap)
            .unwrap_or_else(OrgId::default_org)
    }

    /// Verify `caller` may touch `vm`. Errors with
    /// [`ApiError::Forbidden`] (`cross_org`) when the recorded owner
    /// disagrees. `pub(crate)` because `ApiError` is a crate-internal
    /// enum; route handlers are the only intended callers.
    pub(crate) fn require_vm_access(&self, vm: VmId, caller: &OrgId) -> Result<(), ApiError> {
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

    /// Verify `caller` may touch `snap`. `pub(crate)` for the same
    /// reason as [`require_vm_access`](Self::require_vm_access).
    pub(crate) fn require_snapshot_access(
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
    /// scope.
    #[allow(dead_code)]
    pub fn vms_owned_by(&self, caller: &OrgId) -> HashSet<VmId> {
        self.store.vms_owned_by(caller)
    }

    /// Return the set of snapshot ids owned by `caller`.
    #[allow(dead_code)]
    pub fn snapshots_owned_by(&self, caller: &OrgId) -> HashSet<SnapshotId> {
        self.store.snapshots_owned_by(caller)
    }

    /// `true` when the caller is the default org.
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

    fn map() -> OwnershipMap {
        OwnershipMap::new_in_memory()
    }

    #[test]
    fn record_then_require_passes_for_same_org() {
        let m = map();
        m.record_vm(VmId(1), org("acme"));
        assert!(m.require_vm_access(VmId(1), &org("acme")).is_ok());
    }

    #[test]
    fn require_rejects_cross_org_with_forbidden() {
        let m = map();
        m.record_vm(VmId(1), org("acme"));
        let err = m.require_vm_access(VmId(1), &org("globex")).unwrap_err();
        match err {
            ApiError::Forbidden { code, .. } => assert_eq!(code, "cross_org"),
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }

    #[test]
    fn unrecorded_vm_belongs_to_default_org() {
        let m = map();
        // Nobody recorded VmId(7). Default org is allowed.
        assert!(m.require_vm_access(VmId(7), &OrgId::default_org()).is_ok());
        // A non-default org sees a Forbidden.
        let err = m.require_vm_access(VmId(7), &org("acme")).unwrap_err();
        assert!(matches!(err, ApiError::Forbidden { .. }));
    }

    #[test]
    fn forget_drops_the_recorded_owner() {
        let m = map();
        m.record_vm(VmId(1), org("acme"));
        m.forget_vm(VmId(1));
        // Falls back to default.
        assert_eq!(m.vm_owner(VmId(1)), OrgId::default_org());
    }

    #[test]
    fn record_overwrites_previous_owner() {
        let m = map();
        m.record_vm(VmId(1), org("acme"));
        m.record_vm(VmId(1), org("globex"));
        assert_eq!(m.vm_owner(VmId(1)), org("globex"));
    }

    #[test]
    fn snapshots_independent_from_vms() {
        let m = map();
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
        let m = map();
        m.record_vm(VmId(1), org("acme"));
        m.record_vm(VmId(2), org("acme"));
        m.record_vm(VmId(3), org("globex"));
        let mut acme: Vec<_> = m.vms_owned_by(&org("acme")).into_iter().collect();
        acme.sort_by_key(|id| id.0);
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
