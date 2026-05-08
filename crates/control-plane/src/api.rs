//! Wire-format DTOs for the REST API.
//!
//! The control plane owns its own JSON shape; it converts to/from the
//! `vm-core` types at the handler boundary so that changes to the internal
//! types don't silently mutate the public wire contract.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use vm_core::{SnapshotId, VmConfig, VmHandle, VmId, VmState};

/// Body of `POST /v1/vms`. All fields are optional; missing fields fall back
/// to the same defaults as [`VmConfig::default`].
///
/// When `snapshot_dir` is set the backend reads `manifest.json` from that
/// directory and uses the captured geometry instead of cold-booting from
/// `kernel`/`rootfs`. The `vcpus` / `memory_mib` fields in the request are
/// ignored in that case (the manifest wins).
#[derive(Debug, Deserialize)]
pub(crate) struct CreateVmRequest {
    #[serde(default = "default_vcpus")]
    pub vcpus: u32,
    #[serde(default = "default_memory_mib")]
    pub memory_mib: u64,
    #[serde(default)]
    pub kernel: Option<PathBuf>,
    #[serde(default)]
    pub rootfs: Option<PathBuf>,
    #[serde(default)]
    pub cmdline: String,
    #[serde(default)]
    pub vsock_cid: Option<u32>,
    #[serde(default)]
    pub snapshot_dir: Option<PathBuf>,
}

fn default_vcpus() -> u32 {
    1
}

fn default_memory_mib() -> u64 {
    128
}

impl From<CreateVmRequest> for VmConfig {
    fn from(r: CreateVmRequest) -> Self {
        VmConfig {
            vcpus: r.vcpus,
            memory_mib: r.memory_mib,
            kernel: r.kernel,
            rootfs: r.rootfs,
            cmdline: r.cmdline,
            vsock_cid: r.vsock_cid,
            snapshot_dir: r.snapshot_dir,
        }
    }
}

/// Lifecycle state on the wire. Kept separate from [`VmState`] so we can
/// control the JSON rendering (snake_case) without forcing vm-core to depend
/// on serde.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VmStateDto {
    Created,
    Running,
    Stopped,
}

impl From<VmState> for VmStateDto {
    fn from(s: VmState) -> Self {
        match s {
            VmState::Created => Self::Created,
            VmState::Running => Self::Running,
            VmState::Stopped => Self::Stopped,
        }
    }
}

/// Response body for `POST /v1/vms` and `POST /v1/snapshots/{id}/restore`.
#[derive(Debug, Serialize)]
pub(crate) struct VmHandleDto {
    /// Numeric id. Use this in subsequent URL paths.
    pub id: u64,
    /// Human-readable display form (e.g. `vm-0000000000000042`).
    pub display: String,
    pub state: VmStateDto,
}

impl From<VmHandle> for VmHandleDto {
    fn from(h: VmHandle) -> Self {
        Self {
            id: h.id.0,
            display: h.id.to_string(),
            state: h.state.into(),
        }
    }
}

/// Response body for `GET /v1/vms`. Wraps a list rather than returning a
/// bare JSON array so we can add pagination / filter metadata at the
/// envelope level later without breaking clients.
#[derive(Debug, Serialize)]
pub(crate) struct VmListResponse {
    pub vms: Vec<VmListEntry>,
}

/// Per-VM row in `GET /v1/vms`. Carries the same id + display + state
/// as [`VmHandleDto`] plus the geometry pulled from
/// [`vm_core::Hypervisor::vm_meta`]. Backends that don't track per-VM
/// state (e.g. the placeholder `vm-kvm`) return `Unsupported` and
/// the metadata fields are omitted, leaving id/display/state usable.
#[derive(Debug, Serialize)]
pub(crate) struct VmListEntry {
    pub id: u64,
    pub display: String,
    pub state: VmStateDto,
    /// vCPU count the VM was created with. Absent when the backend
    /// can't surface geometry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u32>,
    /// Guest memory in MiB. Absent when the backend can't surface
    /// geometry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u64>,
    /// Captured kernel command line (empty string when the VM had
    /// none). Absent when the backend can't surface geometry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel_cmdline: Option<String>,
    /// Snapshot directory the VM was restored from, if any. Absent
    /// either when the backend can't surface geometry, or when the VM
    /// was cold-booted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_dir: Option<PathBuf>,
}

impl VmListEntry {
    /// Build a row from the basic [`VmHandle`] fields only — used when
    /// `vm_meta` returned `Unsupported` for this id.
    pub fn id_only(handle: VmHandle) -> Self {
        Self {
            id: handle.id.0,
            display: handle.id.to_string(),
            state: handle.state.into(),
            vcpus: None,
            memory_mib: None,
            kernel_cmdline: None,
            snapshot_dir: None,
        }
    }

    /// Build a row from a [`vm_core::VmMeta`] returned by the backend.
    pub fn from_meta(meta: vm_core::VmMeta) -> Self {
        Self {
            id: meta.id.0,
            display: meta.id.to_string(),
            state: meta.state.into(),
            vcpus: Some(meta.vcpus),
            memory_mib: Some(meta.memory_mib),
            kernel_cmdline: Some(meta.kernel_cmdline),
            snapshot_dir: meta.snapshot_dir,
        }
    }
}

/// Response body for `GET /v1/vms/{id}`.
#[derive(Debug, Serialize)]
pub(crate) struct VmStateResponse {
    pub id: u64,
    pub display: String,
    pub state: VmStateDto,
}

impl VmStateResponse {
    pub fn new(id: VmId, state: VmState) -> Self {
        Self {
            id: id.0,
            display: id.to_string(),
            state: state.into(),
        }
    }
}

/// Optional body for `POST /v1/vms/{id}/snapshot`. The endpoint also
/// accepts an empty body (the legacy in-memory-only behaviour).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct SnapshotRequest {
    /// When set, after capturing the in-memory snapshot the control
    /// plane writes a `snapshot::Manifest` to this directory so the
    /// snapshot can later be restored via the `snapshot_dir` field of
    /// `POST /v1/vms`.
    #[serde(default)]
    pub to_dir: Option<PathBuf>,
}

/// Response body for `POST /v1/vms/{id}/snapshot`. When `to_dir` was
/// supplied in the request, `dir` echoes that path so the client can
/// confirm where the manifest was written.
#[derive(Debug, Serialize)]
pub(crate) struct SnapshotDto {
    pub id: u64,
    pub display: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
}

impl From<SnapshotId> for SnapshotDto {
    fn from(s: SnapshotId) -> Self {
        Self {
            id: s.0,
            display: s.to_string(),
            dir: None,
        }
    }
}

/// Response body for `GET /v1/snapshots`. Wraps the list in an envelope
/// for the same forward-compat reason as [`VmListResponse`] — leaves
/// room for pagination / filter metadata later.
#[derive(Debug, Serialize)]
pub(crate) struct SnapshotListResponse {
    pub snapshots: Vec<SnapshotListEntry>,
}

/// Per-snapshot row in `GET /v1/snapshots`. Carries the same id +
/// display as [`SnapshotDto`] plus the captured geometry pulled from
/// [`vm_core::Hypervisor::snapshot_meta`]. Backends that don't track
/// geometry (e.g. the placeholder `vm-kvm`) return `Unsupported` and
/// the metadata fields are simply omitted, leaving id + display
/// usable.
#[derive(Debug, Serialize)]
pub(crate) struct SnapshotListEntry {
    pub id: u64,
    pub display: String,
    /// vCPU count captured at snapshot time. Absent when the backend
    /// can't surface geometry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcpu_count: Option<u32>,
    /// Guest memory size in bytes. Absent when the backend can't
    /// surface geometry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// Guest page size in bytes. Absent when the backend can't surface
    /// geometry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_size: Option<u32>,
    /// Captured kernel command line (empty string when the VM had
    /// none). Absent when the backend can't surface geometry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel_cmdline: Option<String>,
}

impl SnapshotListEntry {
    /// Build a row that carries id + display only (for backends that
    /// can't surface metadata).
    pub fn id_only(id: SnapshotId) -> Self {
        Self {
            id: id.0,
            display: id.to_string(),
            vcpu_count: None,
            memory_bytes: None,
            page_size: None,
            kernel_cmdline: None,
        }
    }

    /// Build a row from a [`vm_core::SnapshotMeta`] returned by the
    /// backend.
    pub fn from_meta(meta: vm_core::SnapshotMeta) -> Self {
        Self {
            id: meta.id.0,
            display: meta.id.to_string(),
            vcpu_count: Some(meta.vcpu_count),
            memory_bytes: Some(meta.memory_bytes),
            page_size: Some(meta.page_size),
            kernel_cmdline: Some(meta.kernel_cmdline),
        }
    }
}
