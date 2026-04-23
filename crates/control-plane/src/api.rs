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
    pub vms: Vec<VmHandleDto>,
}

impl VmListResponse {
    pub fn new<I>(handles: I) -> Self
    where
        I: IntoIterator<Item = VmHandle>,
    {
        Self {
            vms: handles.into_iter().map(VmHandleDto::from).collect(),
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

/// Response body for `POST /v1/vms/{id}/snapshot`.
#[derive(Debug, Serialize)]
pub(crate) struct SnapshotDto {
    pub id: u64,
    pub display: String,
}

impl From<SnapshotId> for SnapshotDto {
    fn from(s: SnapshotId) -> Self {
        Self {
            id: s.0,
            display: s.to_string(),
        }
    }
}
