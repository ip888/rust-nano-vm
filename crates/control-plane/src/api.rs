//! Wire-format DTOs for the REST API.
//!
//! The control plane owns its own JSON shape; it converts to/from the
//! `vm-core` types at the handler boundary so that changes to the internal
//! types don't silently mutate the public wire contract.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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
            flat_binary: None,
            initrd: None,
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

/// Response body for `POST /v1/snapshots/{id}/fork`. Carries the new VM
/// handle plus the per-fork latency (the headline product number) and the
/// caller's running fork-usage totals so a client can show live billing
/// without a separate `GET /v1/usage` round-trip.
#[derive(Debug, Serialize)]
pub(crate) struct ForkResponseDto {
    pub vm: VmHandleDto,
    /// Wall-time of the fork in milliseconds (server-measured).
    pub fork_ms: u64,
    /// Total successful forks performed by this caller's token.
    pub fork_count: u64,
    /// Sum of `fork_ms` across this caller's history (rough cost basis).
    pub fork_total_ms: u64,
}

/// Response body for `GET /v1/usage` — the caller's per-token fork counts.
/// The token is reported as a non-cryptographic fingerprint so the body is
/// safe to log; the raw bearer never leaves the request.
#[derive(Debug, Serialize)]
pub(crate) struct UsageResponseDto {
    /// `tok-<first4>-<len>` fingerprint of the caller's bearer token.
    pub token: String,
    /// Total successful forks performed by this token.
    pub fork_count: u64,
    /// Sum of per-fork wall-time (ms) charged to this token.
    pub fork_total_ms: u64,
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
/// [`vm_core::Hypervisor::vm_meta`]. Backends that don't expose per-VM
/// geometry return `Unsupported` and the metadata fields are omitted,
/// leaving id/display/state usable.
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
/// [`vm_core::Hypervisor::snapshot_meta`]. Backends that don't expose
/// captured geometry return `Unsupported` and the metadata fields are
/// simply omitted, leaving id + display usable.
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

/// Request body for `POST /v1/vms/{id}/exec`.
#[derive(Debug, Deserialize)]
pub(crate) struct ExecRequest {
    /// Program to execute (absolute path or found on `$PATH`).
    pub program: String,
    /// Argument vector, NOT including `argv[0]`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional working directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Extra environment variables.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Wall-clock timeout in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl From<ExecRequest> for vm_core::GuestExecRequest {
    fn from(r: ExecRequest) -> Self {
        vm_core::GuestExecRequest {
            program: r.program,
            args: r.args,
            cwd: r.cwd,
            env: r.env,
            timeout_ms: r.timeout_ms,
        }
    }
}

/// Response body for `POST /v1/vms/{id}/exec`.
#[derive(Debug, Serialize)]
pub(crate) struct ExecResponse {
    /// Process exit code. `null` when killed by a signal.
    pub exit_code: Option<i32>,
    /// Signal that terminated the process (POSIX). `null` on non-POSIX
    /// or when the process exited normally.
    pub signal: Option<i32>,
    /// Captured standard output (UTF-8; non-UTF-8 bytes are replaced).
    pub stdout: String,
    /// Captured standard error (UTF-8; non-UTF-8 bytes are replaced).
    pub stderr: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

impl From<vm_core::GuestExecResult> for ExecResponse {
    fn from(r: vm_core::GuestExecResult) -> Self {
        Self {
            exit_code: r.exit_code,
            signal: r.signal,
            stdout: String::from_utf8_lossy(&r.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&r.stderr).into_owned(),
            duration_ms: r.duration_ms,
        }
    }
}

/// Request body for `POST /v1/vms/{id}/files`.
#[derive(Debug, Deserialize)]
pub(crate) struct FileWriteRequest {
    /// Absolute path inside the guest (or on the host for the mock backend).
    pub path: String,
    /// Raw file content. JSON array of unsigned bytes (0–255).
    pub content: Vec<u8>,
    /// UNIX permission bits (e.g. 420 for `0o644`). Ignored on non-Unix.
    #[serde(default = "default_file_mode")]
    pub mode: u32,
}

fn default_file_mode() -> u32 {
    0o644
}

/// Response body for `POST /v1/vms/{id}/files`.
#[derive(Debug, Serialize)]
pub(crate) struct FileWrittenResponse {
    /// Number of bytes written.
    pub bytes: u64,
}

/// Response body for `GET /v1/vms/{id}/files`.
#[derive(Debug, Serialize)]
pub(crate) struct FileReadResponse {
    /// Raw file content. JSON array of unsigned bytes (0–255).
    pub content: Vec<u8>,
}

/// Query parameters for `GET /v1/vms/{id}/files`.
#[derive(Debug, Deserialize)]
pub(crate) struct FilePathQuery {
    /// Absolute path inside the guest (or on the host for the mock backend).
    pub path: String,
}

/// OpenAPI 3.1 document for the control-plane REST surface.
pub fn openapi_spec() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "rust-nano-vm control-plane API",
            "version": env!("CARGO_PKG_VERSION")
        },
        "paths": {
            "/healthz": {
                "get": {
                    "summary": "Liveness check",
                    "security": [],
                    "responses": {
                        "200": {
                            "description": "Plain-text health status",
                            "content": {
                                "text/plain": {
                                    "schema": { "type": "string", "example": "ok" }
                                }
                            }
                        }
                    }
                }
            },
            "/openapi.json": {
                "get": {
                    "summary": "OpenAPI contract",
                    "security": [],
                    "responses": {
                        "200": {
                            "description": "OpenAPI 3.1 document",
                            "content": {
                                "application/json": { "schema": { "type": "object" } }
                            }
                        }
                    }
                }
            },
            "/v1/vms": {
                "get": {
                    "summary": "List VMs",
                    "responses": {
                        "200": {
                            "description": "VM list",
                            "content": {
                                "application/json": { "schema": { "$ref": "#/components/schemas/VmListResponse" } }
                            }
                        }
                    }
                },
                "post": {
                    "summary": "Create VM",
                    "requestBody": {
                        "required": false,
                        "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/CreateVmRequest" } }
                        }
                    },
                    "responses": {
                        "201": {
                            "description": "Created VM",
                            "content": {
                                "application/json": { "schema": { "$ref": "#/components/schemas/VmHandleDto" } }
                            }
                        }
                    }
                }
            },
            "/v1/vms/{id}": {
                "get": {
                    "summary": "Get VM state",
                    "parameters": [{ "$ref": "#/components/parameters/VmIdPath" }],
                    "responses": {
                        "200": {
                            "description": "VM state",
                            "content": {
                                "application/json": { "schema": { "$ref": "#/components/schemas/VmStateResponse" } }
                            }
                        }
                    }
                },
                "delete": {
                    "summary": "Destroy VM",
                    "parameters": [{ "$ref": "#/components/parameters/VmIdPath" }],
                    "responses": { "204": { "description": "Destroyed" } }
                }
            },
            "/v1/vms/{id}/start": {
                "post": {
                    "summary": "Start VM",
                    "parameters": [{ "$ref": "#/components/parameters/VmIdPath" }],
                    "responses": { "204": { "description": "Started" } }
                }
            },
            "/v1/vms/{id}/stop": {
                "post": {
                    "summary": "Stop VM",
                    "parameters": [{ "$ref": "#/components/parameters/VmIdPath" }],
                    "responses": { "204": { "description": "Stopped" } }
                }
            },
            "/v1/vms/{id}/snapshot": {
                "post": {
                    "summary": "Create snapshot",
                    "parameters": [{ "$ref": "#/components/parameters/VmIdPath" }],
                    "requestBody": {
                        "required": false,
                        "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/SnapshotRequest" } }
                        }
                    },
                    "responses": {
                        "201": {
                            "description": "Created snapshot",
                            "content": {
                                "application/json": { "schema": { "$ref": "#/components/schemas/SnapshotDto" } }
                            }
                        }
                    }
                }
            },
            "/v1/snapshots": {
                "get": {
                    "summary": "List snapshots",
                    "responses": {
                        "200": {
                            "description": "Snapshot list",
                            "content": {
                                "application/json": { "schema": { "$ref": "#/components/schemas/SnapshotListResponse" } }
                            }
                        }
                    }
                }
            },
            "/v1/snapshots/{id}": {
                "delete": {
                    "summary": "Delete snapshot",
                    "parameters": [{ "$ref": "#/components/parameters/SnapshotIdPath" }],
                    "responses": { "204": { "description": "Deleted" } }
                }
            },
            "/v1/snapshots/{id}/restore": {
                "post": {
                    "summary": "Restore snapshot into VM",
                    "parameters": [{ "$ref": "#/components/parameters/SnapshotIdPath" }],
                    "responses": {
                        "201": {
                            "description": "Created VM from snapshot",
                            "content": {
                                "application/json": { "schema": { "$ref": "#/components/schemas/VmHandleDto" } }
                            }
                        }
                    }
                }
            },
            "/v1/snapshots/{id}/fork": {
                "post": {
                    "summary": "Fork a child VM from a snapshot (metered)",
                    "description": "MAP_PRIVATE CoW fork of the snapshot. Subject to per-token token-bucket quota; throttled callers get 429 with Retry-After.",
                    "parameters": [{ "$ref": "#/components/parameters/SnapshotIdPath" }],
                    "responses": {
                        "201": {
                            "description": "Forked child VM plus per-fork latency and caller usage totals",
                            "content": {
                                "application/json": { "schema": { "$ref": "#/components/schemas/ForkResponse" } }
                            }
                        },
                        "429": {
                            "description": "Rate-limited by per-token fork quota"
                        }
                    }
                }
            },
            "/v1/usage": {
                "get": {
                    "summary": "Caller's per-token fork-usage counters",
                    "responses": {
                        "200": {
                            "description": "Token fingerprint plus running fork totals",
                            "content": {
                                "application/json": { "schema": { "$ref": "#/components/schemas/UsageResponse" } }
                            }
                        }
                    }
                }
            },
            "/v1/vms/{id}/exec": {
                "post": {
                    "summary": "Execute a command in the guest",
                    "parameters": [{ "$ref": "#/components/parameters/VmIdPath" }],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/ExecRequest" } }
                        }
                    },
                    "responses": {
                        "200": {
                            "description": "Command result",
                            "content": {
                                "application/json": { "schema": { "$ref": "#/components/schemas/ExecResponse" } }
                            }
                        }
                    }
                }
            },
            "/v1/vms/{id}/files": {
                "get": {
                    "summary": "Read a file from the guest filesystem",
                    "parameters": [
                        { "$ref": "#/components/parameters/VmIdPath" },
                        {
                            "name": "path",
                            "in": "query",
                            "required": true,
                            "schema": { "type": "string" }
                        }
                    ],
                    "responses": {
                        "200": {
                            "description": "File content",
                            "content": {
                                "application/json": { "schema": { "$ref": "#/components/schemas/FileReadResponse" } }
                            }
                        }
                    }
                },
                "post": {
                    "summary": "Write a file into the guest filesystem",
                    "parameters": [{ "$ref": "#/components/parameters/VmIdPath" }],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/FileWriteRequest" } }
                        }
                    },
                    "responses": {
                        "200": {
                            "description": "Write result",
                            "content": {
                                "application/json": { "schema": { "$ref": "#/components/schemas/FileWrittenResponse" } }
                            }
                        }
                    }
                }
            }
        },
        "components": {
            "securitySchemes": {
                "BearerAuth": {
                    "type": "http",
                    "scheme": "bearer"
                }
            },
            "parameters": {
                "VmIdPath": {
                    "name": "id",
                    "in": "path",
                    "required": true,
                    "schema": { "type": "integer", "minimum": 0 }
                },
                "SnapshotIdPath": {
                    "name": "id",
                    "in": "path",
                    "required": true,
                    "schema": { "type": "integer", "minimum": 0 }
                }
            },
            "schemas": {
                "CreateVmRequest": {
                    "type": "object",
                    "properties": {
                        "vcpus": { "type": "integer", "minimum": 1, "default": 1 },
                        "memory_mib": { "type": "integer", "minimum": 1, "default": 128 },
                        "kernel": { "type": "string" },
                        "rootfs": { "type": "string" },
                        "cmdline": { "type": "string" },
                        "vsock_cid": { "type": "integer", "minimum": 1 },
                        "snapshot_dir": { "type": "string" }
                    }
                },
                "VmStateDto": {
                    "type": "string",
                    "enum": ["created", "running", "stopped"]
                },
                "VmHandleDto": {
                    "type": "object",
                    "required": ["id", "display", "state"],
                    "properties": {
                        "id": { "type": "integer" },
                        "display": { "type": "string" },
                        "state": { "$ref": "#/components/schemas/VmStateDto" }
                    }
                },
                "VmListEntry": {
                    "type": "object",
                    "required": ["id", "display", "state"],
                    "properties": {
                        "id": { "type": "integer" },
                        "display": { "type": "string" },
                        "state": { "$ref": "#/components/schemas/VmStateDto" },
                        "vcpus": { "type": "integer" },
                        "memory_mib": { "type": "integer" },
                        "kernel_cmdline": { "type": "string" },
                        "snapshot_dir": { "type": "string" }
                    }
                },
                "VmListResponse": {
                    "type": "object",
                    "required": ["vms"],
                    "properties": {
                        "vms": {
                            "type": "array",
                            "items": { "$ref": "#/components/schemas/VmListEntry" }
                        }
                    }
                },
                "VmStateResponse": {
                    "type": "object",
                    "required": ["id", "display", "state"],
                    "properties": {
                        "id": { "type": "integer" },
                        "display": { "type": "string" },
                        "state": { "$ref": "#/components/schemas/VmStateDto" }
                    }
                },
                "SnapshotRequest": {
                    "type": "object",
                    "properties": {
                        "to_dir": { "type": "string" }
                    }
                },
                "SnapshotDto": {
                    "type": "object",
                    "required": ["id", "display"],
                    "properties": {
                        "id": { "type": "integer" },
                        "display": { "type": "string" },
                        "dir": { "type": "string" }
                    }
                },
                "ForkResponse": {
                    "type": "object",
                    "required": ["vm", "fork_ms", "fork_count", "fork_total_ms"],
                    "properties": {
                        "vm": { "$ref": "#/components/schemas/VmHandleDto" },
                        "fork_ms": { "type": "integer", "minimum": 0 },
                        "fork_count": { "type": "integer", "minimum": 0 },
                        "fork_total_ms": { "type": "integer", "minimum": 0 }
                    }
                },
                "UsageResponse": {
                    "type": "object",
                    "required": ["token", "fork_count", "fork_total_ms"],
                    "properties": {
                        "token": { "type": "string" },
                        "fork_count": { "type": "integer", "minimum": 0 },
                        "fork_total_ms": { "type": "integer", "minimum": 0 }
                    }
                },
                "SnapshotListEntry": {
                    "type": "object",
                    "required": ["id", "display"],
                    "properties": {
                        "id": { "type": "integer" },
                        "display": { "type": "string" },
                        "vcpu_count": { "type": "integer" },
                        "memory_bytes": { "type": "integer" },
                        "page_size": { "type": "integer" },
                        "kernel_cmdline": { "type": "string" }
                    }
                },
                "SnapshotListResponse": {
                    "type": "object",
                    "required": ["snapshots"],
                    "properties": {
                        "snapshots": {
                            "type": "array",
                            "items": { "$ref": "#/components/schemas/SnapshotListEntry" }
                        }
                    }
                },
                "ExecRequest": {
                    "type": "object",
                    "required": ["program"],
                    "properties": {
                        "program": { "type": "string" },
                        "args": { "type": "array", "items": { "type": "string" } },
                        "cwd": { "type": "string" },
                        "env": {
                            "type": "array",
                            "items": {
                                "type": "array",
                                "prefixItems": [
                                    { "type": "string" },
                                    { "type": "string" }
                                ],
                                "minItems": 2,
                                "maxItems": 2
                            }
                        },
                        "timeout_ms": { "type": "integer", "minimum": 0 }
                    }
                },
                "ExecResponse": {
                    "type": "object",
                    "required": ["stdout", "stderr", "duration_ms"],
                    "properties": {
                        "exit_code": { "type": "integer" },
                        "signal": { "type": "integer" },
                        "stdout": { "type": "string" },
                        "stderr": { "type": "string" },
                        "duration_ms": { "type": "integer", "minimum": 0 }
                    }
                },
                "FileWriteRequest": {
                    "type": "object",
                    "required": ["path", "content"],
                    "properties": {
                        "path": { "type": "string" },
                        "content": {
                            "type": "array",
                            "items": { "type": "integer", "minimum": 0, "maximum": 255 }
                        },
                        "mode": { "type": "integer", "minimum": 0 }
                    }
                },
                "FileWrittenResponse": {
                    "type": "object",
                    "required": ["bytes"],
                    "properties": {
                        "bytes": { "type": "integer", "minimum": 0 }
                    }
                },
                "FileReadResponse": {
                    "type": "object",
                    "required": ["content"],
                    "properties": {
                        "content": {
                            "type": "array",
                            "items": { "type": "integer", "minimum": 0, "maximum": 255 }
                        }
                    }
                }
            }
        }
    })
}
