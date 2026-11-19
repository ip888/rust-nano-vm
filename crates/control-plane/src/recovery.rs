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

const STATE_VERSION: u32 = 2;
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
    Snapshot { snapshot_intent: String },
}

enum VmRecoverySource {
    Cold(VmConfig),
    Snapshot(SnapshotId),
    Failed(RecoveryDecision, String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VmIntent {
    runtime_id: Option<u64>,
    owner: String,
    desired_state: DesiredVmState,
    origin: VmOrigin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotIntent {
    runtime_id: Option<u64>,
    owner: String,
    /// `false` retains this snapshot only as recovery input for dependent VMs;
    /// it is not part of the public snapshot inventory after reconciliation.
    public: bool,
    /// Id used as the durable-store key. `None` means this local snapshot was
    /// never exported and therefore cannot be rebuilt after a process exit.
    durable_store_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecoveryState {
    #[serde(default = "state_version")]
    version: u32,
    #[serde(default)]
    vms: BTreeMap<String, VmIntent>,
    #[serde(default)]
    snapshots: BTreeMap<String, SnapshotIntent>,
    #[serde(default = "next_intent_sequence")]
    next_intent_id: u64,
}

const fn state_version() -> u32 {
    STATE_VERSION
}

const fn next_intent_sequence() -> u64 {
    1
}

impl Default for RecoveryState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            vms: BTreeMap::new(),
            snapshots: BTreeMap::new(),
            next_intent_id: next_intent_sequence(),
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
            let intent_id = allocate_intent_id(file, "vm");
            file.vms.insert(
                intent_id,
                VmIntent {
                    runtime_id: Some(id.0),
                    owner: owner.as_str().to_owned(),
                    desired_state: state.into(),
                    origin: VmOrigin::Cold { config },
                },
            );
            Ok(())
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
            let snapshot_intent = state_file
                .snapshots
                .iter()
                .find(|(_, intent)| intent.runtime_id == Some(source_snapshot.0))
                .map(|(id, _)| id.clone())
                .ok_or_else(|| {
                    RecoveryError::Invalid(format!(
                        "cannot persist VM {}: source snapshot {} has no durable intent",
                        id.0, source_snapshot.0
                    ))
                })?;
            let intent_id = allocate_intent_id(state_file, "vm");
            state_file.vms.insert(
                intent_id,
                VmIntent {
                    runtime_id: Some(id.0),
                    owner: owner.as_str().to_owned(),
                    desired_state: state.into(),
                    origin: VmOrigin::Snapshot { snapshot_intent },
                },
            );
            Ok(())
        })
    }

    pub(crate) fn set_vm_state(&self, id: VmId, state: VmState) -> Result<(), RecoveryError> {
        self.mutate(|file| {
            if let Some(intent) = file.vms.values_mut().find(|v| v.runtime_id == Some(id.0)) {
                intent.desired_state = state.into();
            }
            Ok(())
        })
    }

    pub(crate) fn forget_vm(&self, id: VmId) -> Result<(), RecoveryError> {
        self.mutate(|file| {
            file.vms.retain(|_, intent| intent.runtime_id != Some(id.0));
            let retained: HashSet<String> = file
                .vms
                .values()
                .filter_map(|intent| match &intent.origin {
                    VmOrigin::Snapshot { snapshot_intent } => Some(snapshot_intent.clone()),
                    VmOrigin::Cold { .. } => None,
                })
                .collect();
            file.snapshots
                .retain(|intent_id, intent| intent.public || retained.contains(intent_id));
            Ok(())
        })
    }

    pub(crate) fn record_snapshot(
        &self,
        id: SnapshotId,
        owner: &OrgId,
    ) -> Result<(), RecoveryError> {
        self.mutate(|file| {
            let intent_id = allocate_intent_id(file, "snapshot");
            file.snapshots.insert(
                intent_id,
                SnapshotIntent {
                    runtime_id: Some(id.0),
                    owner: owner.as_str().to_owned(),
                    public: true,
                    durable_store_id: None,
                },
            );
            Ok(())
        })
    }

    pub(crate) fn mark_snapshot_durable(
        &self,
        local_id: SnapshotId,
        durable_id: SnapshotId,
    ) -> Result<(), RecoveryError> {
        self.mutate(|file| {
            if let Some(intent) = file
                .snapshots
                .values_mut()
                .find(|intent| intent.runtime_id == Some(local_id.0))
            {
                intent.durable_store_id = Some(durable_id.0);
            }
            Ok(())
        })
    }

    pub(crate) fn forget_snapshot(&self, id: SnapshotId) -> Result<(), RecoveryError> {
        self.mutate(|file| {
            let Some(intent_id) = file
                .snapshots
                .iter()
                .find(|(_, intent)| intent.runtime_id == Some(id.0))
                .map(|(id, _)| id.clone())
            else {
                return Ok(());
            };
            let has_dependents = file.vms.values().any(|vm| {
                matches!(
                    &vm.origin,
                    VmOrigin::Snapshot { snapshot_intent } if snapshot_intent == &intent_id
                )
            });
            if has_dependents {
                let intent = file
                    .snapshots
                    .get_mut(&intent_id)
                    .expect("snapshot intent found above");
                intent.runtime_id = None;
                intent.public = false;
            } else {
                file.snapshots.remove(&intent_id);
            }
            Ok(())
        })
    }

    fn mutate(
        &self,
        apply: impl FnOnce(&mut RecoveryState) -> Result<(), RecoveryError>,
    ) -> Result<(), RecoveryError> {
        let mut state = self.state.lock().map_err(|_| RecoveryError::Poisoned)?;
        let before = state.clone();
        if let Err(error) = apply(&mut state) {
            *state = before;
            return Err(error);
        }
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

fn allocate_intent_id(state: &mut RecoveryState, kind: &str) -> String {
    let sequence = state.next_intent_id;
    state.next_intent_id = state.next_intent_id.saturating_add(1);
    format!("{kind}-{sequence:020}")
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
        next_intent_id: loaded.next_intent_id,
        ..RecoveryState::default()
    };
    let mut report = RecoveryReport::default();
    let mut snapshot_ids: BTreeMap<String, SnapshotId> = BTreeMap::new();
    let mut snapshot_decisions: BTreeMap<String, RecoveryOutcome> = BTreeMap::new();
    let mut transient_snapshots = Vec::new();
    let mut vm_owners = BTreeMap::new();
    let mut snapshot_owners = BTreeMap::new();

    let available_snapshots: Result<HashSet<u64>, String> = match state.snapshot_store() {
        Some(snapshot_store) => snapshot_store
            .list()
            .map(|ids| ids.into_iter().collect())
            .map_err(|error| error.to_string()),
        None => Err("no durable snapshot store configured".to_owned()),
    };

    for (intent_id, intent) in &loaded.snapshots {
        let previous_id = intent
            .runtime_id
            .or(intent.durable_store_id)
            .unwrap_or_default();
        let Some(durable_id) = intent.durable_store_id else {
            let outcome = RecoveryOutcome {
                kind: RecoveryKind::Snapshot,
                previous_id,
                current_id: None,
                decision: RecoveryDecision::Unrecoverable,
                reason: "snapshot was never exported to a durable store".to_owned(),
            };
            snapshot_decisions.insert(intent_id.clone(), outcome.clone());
            let mut retained = intent.clone();
            retained.runtime_id = None;
            recovered.snapshots.insert(intent_id.clone(), retained);
            report.outcomes.push(outcome);
            continue;
        };

        if let Err(error) = &available_snapshots {
            let outcome = RecoveryOutcome {
                kind: RecoveryKind::Snapshot,
                previous_id,
                current_id: None,
                decision: RecoveryDecision::Unrecoverable,
                reason: format!("snapshot store inventory failed: {error}"),
            };
            snapshot_decisions.insert(intent_id.clone(), outcome.clone());
            let mut retained = intent.clone();
            retained.runtime_id = None;
            recovered.snapshots.insert(intent_id.clone(), retained);
            report.outcomes.push(outcome);
            continue;
        }
        if !available_snapshots
            .as_ref()
            .is_ok_and(|ids| ids.contains(&durable_id))
        {
            let outcome = RecoveryOutcome {
                kind: RecoveryKind::Snapshot,
                previous_id,
                current_id: None,
                decision: RecoveryDecision::Expired,
                reason: format!("durable snapshot {durable_id} is no longer in the store"),
            };
            snapshot_decisions.insert(intent_id.clone(), outcome.clone());
            let mut retained = intent.clone();
            retained.runtime_id = None;
            recovered.snapshots.insert(intent_id.clone(), retained);
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
                snapshot_ids.insert(intent_id.clone(), local_id);
                let current_id = if intent.public {
                    snapshot_owners.insert(local_id, owner);
                    Some(local_id.0)
                } else {
                    transient_snapshots.push(local_id);
                    None
                };
                let outcome = RecoveryOutcome {
                    kind: RecoveryKind::Snapshot,
                    previous_id,
                    current_id,
                    decision: RecoveryDecision::Recreated,
                    reason: if intent.public {
                        "adopted from the durable snapshot store".to_owned()
                    } else {
                        "temporarily adopted as retained VM recovery input".to_owned()
                    },
                };
                snapshot_decisions.insert(intent_id.clone(), outcome.clone());
                let mut recovered_intent = intent.clone();
                recovered_intent.runtime_id = if intent.public {
                    Some(local_id.0)
                } else {
                    None
                };
                recovered
                    .snapshots
                    .insert(intent_id.clone(), recovered_intent);
                report.outcomes.push(outcome);
            }
            Err((decision, error)) => {
                let outcome = RecoveryOutcome {
                    kind: RecoveryKind::Snapshot,
                    previous_id,
                    current_id: None,
                    decision,
                    reason: format!("durable snapshot adoption failed: {error}"),
                };
                snapshot_decisions.insert(intent_id.clone(), outcome.clone());
                let mut retained = intent.clone();
                retained.runtime_id = None;
                recovered.snapshots.insert(intent_id.clone(), retained);
                report.outcomes.push(outcome);
            }
        }
    }

    for (intent_id, intent) in &loaded.vms {
        let previous_id = intent.runtime_id.unwrap_or_default();
        let origin = match &intent.origin {
            VmOrigin::Cold { config } => {
                let mut config = config.clone();
                for forward in &mut config.port_forwards {
                    forward.host_port = 0;
                }
                VmRecoverySource::Cold(config)
            }
            VmOrigin::Snapshot { snapshot_intent } => {
                if let Some(local_id) = snapshot_ids.get(snapshot_intent) {
                    VmRecoverySource::Snapshot(*local_id)
                } else if let Some(outcome) = snapshot_decisions.get(snapshot_intent) {
                    VmRecoverySource::Failed(
                        outcome.decision,
                        format!(
                            "source snapshot intent {snapshot_intent} was not recovered: {}",
                            outcome.reason
                        ),
                    )
                } else {
                    VmRecoverySource::Failed(
                        RecoveryDecision::Unrecoverable,
                        format!("source snapshot intent {snapshot_intent} is absent"),
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
            vm_owners.insert(handle.id, owner);
            state.record_recovered_endpoints(&handle);
            let mut recovered_intent = intent.clone();
            recovered_intent.runtime_id = Some(handle.id.0);
            recovered.vms.insert(intent_id.clone(), recovered_intent);
            report.outcomes.push(RecoveryOutcome {
                kind: RecoveryKind::Vm,
                previous_id,
                current_id: Some(handle.id.0),
                decision,
                reason,
            });
        } else {
            let mut retained = intent.clone();
            retained.runtime_id = None;
            recovered.vms.insert(intent_id.clone(), retained);
            report.outcomes.push(RecoveryOutcome {
                kind: RecoveryKind::Vm,
                previous_id,
                current_id: None,
                decision,
                reason,
            });
        }
    }

    for snapshot in transient_snapshots {
        state
            .hypervisor()
            .delete_snapshot(snapshot)
            .map_err(|error| {
                RecoveryError::Invalid(format!(
                    "could not remove transient recovery snapshot {snapshot}: {error}"
                ))
            })?;
    }

    let recovered_vm_ids: HashSet<_> = vm_owners.keys().copied().collect();
    for intent in loaded.vms.values() {
        let Some(old_runtime_id) = intent.runtime_id else {
            continue;
        };
        let old_id = VmId(old_runtime_id);
        if !recovered_vm_ids.contains(&old_id) {
            state
                .ownership()
                .try_forget_vm(old_id)
                .map_err(|error| RecoveryError::Invalid(error.to_string()))?;
        }
    }
    for (id, owner) in vm_owners {
        state
            .ownership()
            .try_record_vm(id, &owner)
            .map_err(|error| RecoveryError::Invalid(error.to_string()))?;
    }

    let recovered_snapshot_ids: HashSet<_> = snapshot_owners.keys().copied().collect();
    for old_id in loaded
        .snapshots
        .values()
        .filter_map(|intent| intent.runtime_id)
    {
        if !recovered_snapshot_ids.contains(&SnapshotId(old_id)) {
            state
                .ownership()
                .try_forget_snapshot(SnapshotId(old_id))
                .map_err(|error| RecoveryError::Invalid(error.to_string()))?;
        }
    }
    for (id, owner) in snapshot_owners {
        state
            .ownership()
            .try_record_snapshot(id, &owner)
            .map_err(|error| RecoveryError::Invalid(error.to_string()))?;
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
        DesiredVmState::Stopped if handle.state == VmState::Created => state
            .hypervisor()
            .start(handle.id)
            .and_then(|()| state.hypervisor().stop(handle.id))
            .map(|()| {
                handle.state = VmState::Stopped;
            }),
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
        assert_eq!(
            state
                .vms
                .values()
                .find(|intent| intent.runtime_id == Some(7))
                .unwrap()
                .owner,
            "acme"
        );
        assert_eq!(
            state
                .snapshots
                .values()
                .find(|intent| intent.runtime_id == Some(11))
                .unwrap()
                .durable_store_id,
            Some(101)
        );
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

    #[test]
    fn deleted_public_snapshot_remains_as_stable_vm_recovery_source() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::open(temp.path().join("state.json")).unwrap();
        let owner = OrgId::new("acme");
        store.record_snapshot(SnapshotId(9), &owner).unwrap();
        store
            .mark_snapshot_durable(SnapshotId(9), SnapshotId(109))
            .unwrap();
        store
            .record_snapshot_vm(VmId(5), SnapshotId(9), VmState::Running, &owner)
            .unwrap();
        store.forget_snapshot(SnapshotId(9)).unwrap();

        let state = store.state.lock().unwrap();
        let snapshot = state.snapshots.values().next().unwrap();
        assert!(!snapshot.public);
        assert_eq!(snapshot.runtime_id, None);
        assert_eq!(snapshot.durable_store_id, Some(109));
        assert!(state.vms.values().any(|vm| {
            matches!(
                &vm.origin,
                VmOrigin::Snapshot { snapshot_intent }
                    if state.snapshots.contains_key(snapshot_intent)
            )
        }));
    }

    #[test]
    fn stopped_cold_vm_reaches_stopped_not_created() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(RecoveryStore::open(temp.path().join("state.json")).unwrap());
        store
            .record_cold_vm(
                VmId(4),
                VmConfig::default(),
                VmState::Stopped,
                &OrgId::default_org(),
            )
            .unwrap();
        let hypervisor: Arc<dyn Hypervisor> = Arc::new(vm_mock::MockHypervisor::new());
        let state = AppState::new(hypervisor.clone()).with_recovery_store(store);

        let report = state.reconcile_restart().unwrap();
        let recovered_id = VmId(report.outcomes[0].current_id.unwrap());
        assert_eq!(hypervisor.state(recovered_id).unwrap(), VmState::Stopped);
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
