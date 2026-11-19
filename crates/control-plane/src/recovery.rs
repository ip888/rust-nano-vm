//! Durable one-node control-plane intent and restart reconciliation.
//!
//! Backend VM and snapshot ids, worker processes, and host ports are
//! process-local. The recovery journal deliberately does not persist any of
//! those as authoritative state. It persists the intent needed to recreate
//! resources, then records the fresh ids assigned by the hypervisor after a
//! successful restart reconciliation.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use vm_core::{SnapshotId, VmConfig, VmHandle, VmId, VmState};

use crate::auth::OrgId;
use crate::routes::AppState;

/// Environment variable selecting the durable recovery-journal file.
///
/// The journal is intended for one control-plane replica backed by one local
/// data volume. Multi-replica coordination is deliberately out of scope.
pub const ENV_STATE_PATH: &str = "NANOVM_RECOVERY_STATE_PATH";

const STATE_VERSION: u32 = 1;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DesiredVmState {
    Created,
    Running,
    Stopped,
}

impl From<VmState> for DesiredVmState {
    fn from(value: VmState) -> Self {
        match value {
            VmState::Created => Self::Created,
            VmState::Running => Self::Running,
            VmState::Stopped => Self::Stopped,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum VmOrigin {
    Cold { config: VmConfig },
    Snapshot { snapshot_id: u64 },
}

enum VmRecoverySource {
    Cold(VmConfig),
    Snapshot(SnapshotId),
    Failed(RecoveryDecision, String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VmIntent {
    owner: String,
    desired_state: DesiredVmState,
    origin: VmOrigin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotIntent {
    owner: String,
    /// Id used as the durable-store key. `None` means this local snapshot was
    /// never exported and therefore cannot be rebuilt after a process exit.
    durable_store_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecoveryState {
    #[serde(default = "state_version")]
    version: u32,
    #[serde(default)]
    vms: BTreeMap<u64, VmIntent>,
    #[serde(default)]
    snapshots: BTreeMap<u64, SnapshotIntent>,
}

const fn state_version() -> u32 {
    STATE_VERSION
}

impl Default for RecoveryState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            vms: BTreeMap::new(),
            snapshots: BTreeMap::new(),
        }
    }
}

/// Errors reading or persisting the recovery journal.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// The state file could not be read or atomically replaced.
    #[error("recovery journal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// The state file does not contain the supported journal shape.
    #[error("recovery journal is invalid: {0}")]
    Invalid(String),
    /// An internal lock required to access the journal was poisoned.
    #[error("recovery journal lock poisoned")]
    Poisoned,
}

/// Persistent journal of VM and snapshot intent for one control-plane node.
#[derive(Debug)]
pub struct RecoveryStore {
    path: PathBuf,
    state: Mutex<RecoveryState>,
}

impl RecoveryStore {
    /// Open a journal, creating an empty durable state if `path` is absent.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, RecoveryError> {
        let path = path.into();
        let state = match fs::read(&path) {
            Ok(bytes) => {
                let state: RecoveryState = serde_json::from_slice(&bytes)
                    .map_err(|error| RecoveryError::Invalid(error.to_string()))?;
                if state.version != STATE_VERSION {
                    return Err(RecoveryError::Invalid(format!(
                        "unsupported state version {}, expected {STATE_VERSION}",
                        state.version
                    )));
                }
                state
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => RecoveryState::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    /// Construct from [`ENV_STATE_PATH`]. An unset or empty value disables
    /// durable recovery and retains the historical in-memory behaviour.
    pub fn from_env() -> Result<Option<Self>, RecoveryError> {
        match std::env::var(ENV_STATE_PATH) {
            Ok(path) if !path.trim().is_empty() => Ok(Some(Self::open(path)?)),
            Ok(_) => Ok(None),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(value)) => Err(RecoveryError::Invalid(format!(
                "{ENV_STATE_PATH} is not valid UTF-8: {value:?}"
            ))),
        }
    }

    /// Path backing this journal.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn record_cold_vm(
        &self,
        id: VmId,
        config: VmConfig,
        state: VmState,
        owner: &OrgId,
    ) -> Result<(), RecoveryError> {
        self.mutate(|file| {
            file.vms.insert(
                id.0,
                VmIntent {
                    owner: owner.as_str().to_owned(),
                    desired_state: state.into(),
                    origin: VmOrigin::Cold { config },
                },
            );
        })
    }

    pub(crate) fn record_snapshot_vm(
        &self,
        id: VmId,
        source_snapshot: SnapshotId,
        state: VmState,
        owner: &OrgId,
    ) -> Result<(), RecoveryError> {
        self.mutate(|state_file| {
            state_file.vms.insert(
                id.0,
                VmIntent {
                    owner: owner.as_str().to_owned(),
                    desired_state: state.into(),
                    origin: VmOrigin::Snapshot {
                        snapshot_id: source_snapshot.0,
                    },
                },
            );
        })
    }

    pub(crate) fn set_vm_state(&self, id: VmId, state: VmState) -> Result<(), RecoveryError> {
        self.mutate(|file| {
            if let Some(intent) = file.vms.get_mut(&id.0) {
                intent.desired_state = state.into();
            }
        })
    }

    pub(crate) fn forget_vm(&self, id: VmId) -> Result<(), RecoveryError> {
        self.mutate(|state| {
            state.vms.remove(&id.0);
        })
    }

    pub(crate) fn record_snapshot(
        &self,
        id: SnapshotId,
        owner: &OrgId,
    ) -> Result<(), RecoveryError> {
        self.mutate(|file| {
            file.snapshots.insert(
                id.0,
                SnapshotIntent {
                    owner: owner.as_str().to_owned(),
                    durable_store_id: None,
                },
            );
        })
    }

    pub(crate) fn mark_snapshot_durable(
        &self,
        local_id: SnapshotId,
        durable_id: SnapshotId,
    ) -> Result<(), RecoveryError> {
        self.mutate(|file| {
            if let Some(intent) = file.snapshots.get_mut(&local_id.0) {
                intent.durable_store_id = Some(durable_id.0);
            }
        })
    }

    pub(crate) fn forget_snapshot(&self, id: SnapshotId) -> Result<(), RecoveryError> {
        self.mutate(|state| {
            state.snapshots.remove(&id.0);
        })
    }

    fn mutate(&self, apply: impl FnOnce(&mut RecoveryState)) -> Result<(), RecoveryError> {
        let mut state = self.state.lock().map_err(|_| RecoveryError::Poisoned)?;
        let before = state.clone();
        apply(&mut state);
        if let Err(error) = persist(&self.path, &state) {
            *state = before;
            return Err(error);
        }
        Ok(())
    }

    fn replace_after_recovery(&self, recovered: RecoveryState) -> Result<(), RecoveryError> {
        let mut state = self.state.lock().map_err(|_| RecoveryError::Poisoned)?;
        persist(&self.path, &recovered)?;
        *state = recovered;
        Ok(())
    }
}

fn persist(path: &Path, state: &RecoveryState) -> Result<(), RecoveryError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        sequence
    ));
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|error| RecoveryError::Invalid(error.to_string()))?;

    let result: Result<(), RecoveryError> = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp_path, path)?;
        if let Ok(directory) = fs::File::open(parent) {
            directory.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

/// Resource class included in a recovery report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryKind {
    /// A VM intent.
    Vm,
    /// A snapshot intent.
    Snapshot,
}

/// Classification applied to one persisted resource during startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryDecision {
    /// The resource was recreated from valid intent and fresh runtime state.
    Recreated,
    /// The durable snapshot object is no longer present in the configured
    /// store, so an intent depending on it has naturally expired.
    Expired,
    /// The intent could not be recreated because it was never made durable,
    /// the store is unavailable, or the hypervisor rejected recreation.
    Unrecoverable,
}

/// One line-item in a [`RecoveryReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoveryOutcome {
    /// Whether the item is a VM or snapshot.
    pub kind: RecoveryKind,
    /// Resource id persisted by the previous process.
    pub previous_id: u64,
    /// Fresh backend id assigned during recovery, when recreation succeeded.
    pub current_id: Option<u64>,
    /// Result classification.
    pub decision: RecoveryDecision,
    /// Operator-actionable explanation.
    pub reason: String,
}

/// Complete result of startup reconciliation.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RecoveryReport {
    /// One result for every durable VM and snapshot intent.
    pub outcomes: Vec<RecoveryOutcome>,
}

impl RecoveryReport {
    /// Number of successfully reconstructed resources.
    pub fn recreated(&self) -> usize {
        self.count(RecoveryDecision::Recreated)
    }

    /// Number of resources whose durable snapshot expired.
    pub fn expired(&self) -> usize {
        self.count(RecoveryDecision::Expired)
    }

    /// Number of resources that require operator attention or remediation.
    pub fn unrecoverable(&self) -> usize {
        self.count(RecoveryDecision::Unrecoverable)
    }

    fn count(&self, decision: RecoveryDecision) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.decision == decision)
            .count()
    }
}

pub(crate) fn reconcile(
    state: &AppState,
    store: &RecoveryStore,
) -> Result<RecoveryReport, RecoveryError> {
    let loaded = store
        .state
        .lock()
        .map_err(|_| RecoveryError::Poisoned)?
        .clone();
    let mut recovered = RecoveryState {
        version: STATE_VERSION,
        ..RecoveryState::default()
    };
    let mut report = RecoveryReport::default();
    let mut snapshot_ids = BTreeMap::new();
    let mut snapshot_decisions = BTreeMap::new();

    let available_snapshots: Result<HashSet<u64>, String> = match state.snapshot_store() {
        Some(snapshot_store) => snapshot_store
            .list()
            .map(|ids| ids.into_iter().collect())
            .map_err(|error| error.to_string()),
        None => Err("no durable snapshot store configured".to_owned()),
    };

    for (previous_id, intent) in &loaded.snapshots {
        let Some(durable_id) = intent.durable_store_id else {
            let outcome = RecoveryOutcome {
                kind: RecoveryKind::Snapshot,
                previous_id: *previous_id,
                current_id: None,
                decision: RecoveryDecision::Unrecoverable,
                reason: "snapshot was never exported to a durable store".to_owned(),
            };
            snapshot_decisions.insert(*previous_id, outcome.clone());
            recovered
                .snapshots
                .entry(*previous_id)
                .or_insert_with(|| intent.clone());
            report.outcomes.push(outcome);
            continue;
        };

        if let Err(error) = &available_snapshots {
            let outcome = RecoveryOutcome {
                kind: RecoveryKind::Snapshot,
                previous_id: *previous_id,
                current_id: None,
                decision: RecoveryDecision::Unrecoverable,
                reason: format!("snapshot store inventory failed: {error}"),
            };
            snapshot_decisions.insert(*previous_id, outcome.clone());
            recovered
                .snapshots
                .entry(*previous_id)
                .or_insert_with(|| intent.clone());
            report.outcomes.push(outcome);
            continue;
        }
        if !available_snapshots
            .as_ref()
            .is_ok_and(|ids| ids.contains(&durable_id))
        {
            let outcome = RecoveryOutcome {
                kind: RecoveryKind::Snapshot,
                previous_id: *previous_id,
                current_id: None,
                decision: RecoveryDecision::Expired,
                reason: format!("durable snapshot {durable_id} is no longer in the store"),
            };
            snapshot_decisions.insert(*previous_id, outcome.clone());
            recovered
                .snapshots
                .entry(*previous_id)
                .or_insert_with(|| intent.clone());
            report.outcomes.push(outcome);
            continue;
        }

        let target = recovery_temp_dir(durable_id);
        let snapshot_store = state
            .snapshot_store()
            .expect("available snapshot inventory requires a store");
        let adopted: Result<SnapshotId, (RecoveryDecision, String)> =
            match snapshot_store.get(durable_id, &target) {
                Ok(()) => match snapshot::Manifest::read_from_dir(&target) {
                    Ok(manifest) if manifest.snapshot_id != durable_id => Err((
                        RecoveryDecision::Unrecoverable,
                        format!(
                            "manifest id {} did not match durable store id {durable_id}",
                            manifest.snapshot_id
                        ),
                    )),
                    Ok(_) => state
                        .hypervisor()
                        .snapshot_adopt(&target)
                        .map_err(|error| (RecoveryDecision::Unrecoverable, error.to_string())),
                    Err(error) => Err((RecoveryDecision::Unrecoverable, error.to_string())),
                },
                Err(snapshot::StoreError::NotFound(id)) => Err((
                    RecoveryDecision::Expired,
                    format!("durable snapshot {id} disappeared during download"),
                )),
                Err(error) => Err((RecoveryDecision::Unrecoverable, error.to_string())),
            };
        let _ = fs::remove_dir_all(&target);

        match adopted {
            Ok(local_id) => {
                let owner = OrgId::new(&intent.owner);
                state.ownership().record_snapshot(local_id, owner);
                if local_id.0 != *previous_id {
                    state.ownership().forget_snapshot(SnapshotId(*previous_id));
                }
                snapshot_ids.insert(*previous_id, local_id);
                snapshot_decisions.insert(
                    *previous_id,
                    RecoveryOutcome {
                        kind: RecoveryKind::Snapshot,
                        previous_id: *previous_id,
                        current_id: Some(local_id.0),
                        decision: RecoveryDecision::Recreated,
                        reason: "adopted from the durable snapshot store".to_owned(),
                    },
                );
                recovered.snapshots.insert(local_id.0, intent.clone());
                report.outcomes.push(RecoveryOutcome {
                    kind: RecoveryKind::Snapshot,
                    previous_id: *previous_id,
                    current_id: Some(local_id.0),
                    decision: RecoveryDecision::Recreated,
                    reason: "adopted from the durable snapshot store".to_owned(),
                });
            }
            Err((decision, error)) => {
                let outcome = RecoveryOutcome {
                    kind: RecoveryKind::Snapshot,
                    previous_id: *previous_id,
                    current_id: None,
                    decision,
                    reason: format!("durable snapshot adoption failed: {error}"),
                };
                snapshot_decisions.insert(*previous_id, outcome.clone());
                recovered
                    .snapshots
                    .entry(*previous_id)
                    .or_insert_with(|| intent.clone());
                report.outcomes.push(outcome);
            }
        }
    }

    for (previous_id, intent) in &loaded.vms {
        let origin = match &intent.origin {
            VmOrigin::Cold { config } => {
                let mut config = config.clone();
                for forward in &mut config.port_forwards {
                    forward.host_port = 0;
                }
                VmRecoverySource::Cold(config)
            }
            VmOrigin::Snapshot { snapshot_id } => {
                if let Some(local_id) = snapshot_ids.get(snapshot_id) {
                    VmRecoverySource::Snapshot(*local_id)
                } else if let Some(outcome) = snapshot_decisions.get(snapshot_id) {
                    VmRecoverySource::Failed(
                        outcome.decision,
                        format!(
                            "source snapshot {snapshot_id} was not recovered: {}",
                            outcome.reason
                        ),
                    )
                } else {
                    VmRecoverySource::Failed(
                        RecoveryDecision::Unrecoverable,
                        format!("source snapshot {snapshot_id} is absent from durable intent"),
                    )
                }
            }
        };

        let (decision, reason, handle) = match origin {
            VmRecoverySource::Cold(config) => match state.hypervisor().create_vm(&config) {
                Ok(handle) => recover_desired_state(state, handle, &intent.desired_state),
                Err(error) => (
                    RecoveryDecision::Unrecoverable,
                    format!("cold VM recreation failed: {error}"),
                    None,
                ),
            },
            VmRecoverySource::Snapshot(local_id) => match state.hypervisor().restore(local_id) {
                Ok(handle) => recover_desired_state(state, handle, &intent.desired_state),
                Err(error) => (
                    RecoveryDecision::Unrecoverable,
                    format!("snapshot VM recreation failed: {error}"),
                    None,
                ),
            },
            VmRecoverySource::Failed(decision, reason) => (decision, reason, None),
        };

        if let Some(handle) = handle {
            let owner = OrgId::new(&intent.owner);
            state.ownership().record_vm(handle.id, owner);
            if handle.id.0 != *previous_id {
                state.ownership().forget_vm(VmId(*previous_id));
            }
            state.record_recovered_endpoints(&handle);
            recovered.vms.insert(
                handle.id.0,
                VmIntent {
                    owner: intent.owner.clone(),
                    desired_state: intent.desired_state.clone(),
                    origin: remap_origin(&intent.origin, &snapshot_ids),
                },
            );
            report.outcomes.push(RecoveryOutcome {
                kind: RecoveryKind::Vm,
                previous_id: *previous_id,
                current_id: Some(handle.id.0),
                decision,
                reason,
            });
        } else {
            recovered
                .vms
                .entry(*previous_id)
                .or_insert_with(|| intent.clone());
            report.outcomes.push(RecoveryOutcome {
                kind: RecoveryKind::Vm,
                previous_id: *previous_id,
                current_id: None,
                decision,
                reason,
            });
        }
    }

    store.replace_after_recovery(recovered)?;
    Ok(report)
}

fn recover_desired_state(
    state: &AppState,
    mut handle: VmHandle,
    desired: &DesiredVmState,
) -> (RecoveryDecision, String, Option<VmHandle>) {
    let transition = match desired {
        DesiredVmState::Running if handle.state != VmState::Running => {
            state.hypervisor().start(handle.id).map(|()| {
                handle.state = VmState::Running;
            })
        }
        DesiredVmState::Created | DesiredVmState::Stopped if handle.state == VmState::Running => {
            state.hypervisor().stop(handle.id).map(|()| {
                handle.state = VmState::Stopped;
            })
        }
        _ => Ok(()),
    };
    if let Err(error) = transition {
        let _ = state.hypervisor().destroy(handle.id);
        return (
            RecoveryDecision::Unrecoverable,
            format!("desired VM state could not be restored: {error}"),
            None,
        );
    }

    // Prefer a fresh authoritative handle after lifecycle transitions. If
    // the backend cannot list, the create/restore handle is still fresh and
    // contains its newly-bound endpoints.
    if let Ok(handles) = state.hypervisor().list_vms() {
        if let Some(fresh) = handles
            .into_iter()
            .find(|candidate| candidate.id == handle.id)
        {
            let previous_endpoints = handle.endpoints;
            handle = fresh;
            if handle.endpoints.is_empty() {
                handle.endpoints = previous_endpoints;
            }
        }
    }
    (
        RecoveryDecision::Recreated,
        "recreated with fresh backend state and endpoints".to_owned(),
        Some(handle),
    )
}

fn remap_origin(origin: &VmOrigin, snapshots: &BTreeMap<u64, SnapshotId>) -> VmOrigin {
    match origin {
        VmOrigin::Cold { config } => VmOrigin::Cold {
            config: config.clone(),
        },
        VmOrigin::Snapshot { snapshot_id } => VmOrigin::Snapshot {
            snapshot_id: snapshots
                .get(snapshot_id)
                .map(|id| id.0)
                .unwrap_or(*snapshot_id),
        },
    }
}

fn recovery_temp_dir(snapshot_id: u64) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "nanovm-recovery-{}-{snapshot_id}-{sequence}",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppState, RecoveryDecision};
    use std::sync::Arc;
    use vm_core::{Hypervisor, PortForward, SnapshotMeta, VmMeta, VmResult};

    #[test]
    fn journal_round_trips_vm_and_snapshot_intent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        let store = RecoveryStore::open(&path).unwrap();
        let owner = OrgId::new("acme");
        store
            .record_cold_vm(VmId(7), VmConfig::default(), VmState::Running, &owner)
            .unwrap();
        store.record_snapshot(SnapshotId(11), &owner).unwrap();
        store
            .mark_snapshot_durable(SnapshotId(11), SnapshotId(101))
            .unwrap();
        drop(store);

        let reopened = RecoveryStore::open(&path).unwrap();
        let state = reopened.state.lock().unwrap();
        assert_eq!(state.vms[&7].owner, "acme");
        assert_eq!(state.snapshots[&11].durable_store_id, Some(101));
    }

    #[test]
    fn report_counts_each_decision() {
        let report = RecoveryReport {
            outcomes: vec![
                outcome(1, RecoveryDecision::Recreated),
                outcome(2, RecoveryDecision::Expired),
                outcome(3, RecoveryDecision::Unrecoverable),
            ],
        };
        assert_eq!(report.recreated(), 1);
        assert_eq!(report.expired(), 1);
        assert_eq!(report.unrecoverable(), 1);
    }

    #[test]
    fn recovery_forces_fresh_host_port_allocation() {
        #[derive(Debug)]
        struct TrackingHypervisor {
            inner: vm_mock::MockHypervisor,
            configs: Mutex<Vec<VmConfig>>,
        }

        impl TrackingHypervisor {
            fn new() -> Self {
                Self {
                    inner: vm_mock::MockHypervisor::new(),
                    configs: Mutex::new(Vec::new()),
                }
            }
        }

        impl Hypervisor for TrackingHypervisor {
            fn create_vm(&self, config: &VmConfig) -> VmResult<VmHandle> {
                self.configs.lock().unwrap().push(config.clone());
                self.inner.create_vm(config)
            }
            fn start(&self, id: VmId) -> VmResult<()> {
                self.inner.start(id)
            }
            fn stop(&self, id: VmId) -> VmResult<()> {
                self.inner.stop(id)
            }
            fn state(&self, id: VmId) -> VmResult<VmState> {
                self.inner.state(id)
            }
            fn snapshot(&self, id: VmId) -> VmResult<SnapshotId> {
                self.inner.snapshot(id)
            }
            fn restore(&self, snapshot: SnapshotId) -> VmResult<VmHandle> {
                self.inner.restore(snapshot)
            }
            fn destroy(&self, id: VmId) -> VmResult<()> {
                self.inner.destroy(id)
            }
            fn list_vms(&self) -> VmResult<Vec<VmHandle>> {
                self.inner.list_vms()
            }
            fn list_snapshots(&self) -> VmResult<Vec<SnapshotId>> {
                self.inner.list_snapshots()
            }
            fn delete_snapshot(&self, snapshot: SnapshotId) -> VmResult<()> {
                self.inner.delete_snapshot(snapshot)
            }
            fn snapshot_meta(&self, snapshot: SnapshotId) -> VmResult<SnapshotMeta> {
                self.inner.snapshot_meta(snapshot)
            }
            fn snapshot_export_dir(&self, snapshot: SnapshotId) -> VmResult<Option<PathBuf>> {
                self.inner.snapshot_export_dir(snapshot)
            }
            fn snapshot_adopt(&self, dir: &Path) -> VmResult<SnapshotId> {
                self.inner.snapshot_adopt(dir)
            }
            fn vm_meta(&self, id: VmId) -> VmResult<VmMeta> {
                self.inner.vm_meta(id)
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(RecoveryStore::open(temp.path().join("state.json")).unwrap());
        let mut config = VmConfig::default();
        config.port_forwards.push(PortForward {
            guest_port: 8080,
            host_address: "127.0.0.1".into(),
            host_port: 43123,
        });
        let owner = OrgId::new("acme");
        store
            .record_cold_vm(VmId(8), config, VmState::Running, &owner)
            .unwrap();

        let hypervisor = Arc::new(TrackingHypervisor::new());
        let state = AppState::new(hypervisor.clone()).with_recovery_store(store);
        let report = state.reconcile_restart().unwrap();

        assert_eq!(report.recreated(), 1);
        assert_eq!(
            hypervisor.configs.lock().unwrap()[0].port_forwards[0].host_port,
            0
        );
        let current_id = VmId(report.outcomes[0].current_id.unwrap());
        assert!(state
            .ownership()
            .require_vm_access(current_id, &owner)
            .is_ok());
        assert!(state
            .ownership()
            .require_vm_access(current_id, &OrgId::default_org())
            .is_err());
    }

    #[test]
    fn missing_durable_snapshot_expires_dependent_intents() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(RecoveryStore::open(temp.path().join("state.json")).unwrap());
        let owner = OrgId::default_org();
        store.record_snapshot(SnapshotId(9), &owner).unwrap();
        store
            .mark_snapshot_durable(SnapshotId(9), SnapshotId(109))
            .unwrap();
        store
            .record_snapshot_vm(VmId(5), SnapshotId(9), VmState::Running, &owner)
            .unwrap();
        let snapshot_store: Arc<dyn snapshot::SnapshotStore> = Arc::new(
            snapshot::FsSnapshotStore::new(temp.path().join("snapshots")),
        );
        let hypervisor: Arc<dyn Hypervisor> = Arc::new(vm_mock::MockHypervisor::new());
        let state = AppState::new(hypervisor)
            .with_snapshot_store(Some(snapshot_store))
            .with_recovery_store(store);

        let report = state.reconcile_restart().unwrap();

        assert_eq!(report.expired(), 2);
        assert_eq!(report.unrecoverable(), 0);
    }

    fn outcome(previous_id: u64, decision: RecoveryDecision) -> RecoveryOutcome {
        RecoveryOutcome {
            kind: RecoveryKind::Vm,
            previous_id,
            current_id: None,
            decision,
            reason: String::new(),
        }
    }
}
