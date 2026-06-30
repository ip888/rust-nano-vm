//! Wire contract between the rust-nano-vm control-plane orchestrator
//! and the per-VM `nanovm-vmm-child` workers.
//!
//! # Why this crate exists
//!
//! The control plane today runs every VM inside a single VMM process.
//! That makes the cgroups v2 caps in `vm-kvm::cgroups` *process-wide*:
//! all VMs share one `memory.max` and one `cpu.max`. For honest
//! multi-tenant isolation we need per-VM caps — and the cgroups v2
//! kernel design only enforces memory accounting per *process*, so the
//! right shape is one VMM process per VM.
//!
//! `vmm-ipc` is the wire shape on the boundary. The orchestrator
//! (running inside `nanovm-control-plane`) spawns a `nanovm-vmm-child`
//! per VM, hands the child a Unix socket, and drives the VM via a
//! tagged-union request/response protocol over that socket.
//!
//! # Design choices
//!
//! - **Framing.** Length-prefixed payloads: 4-byte big-endian frame
//!   length (`u32`), then payload bytes. Simple to parse, no
//!   look-ahead, no delimiter-escaping. Max frame is 4 MiB by default
//!   (configurable on the reader); requests with bigger payloads
//!   (e.g. a large `write_file`) must stream them another way.
//!
//! - **Encoding.** Newline-free JSON inside each frame. Two reasons:
//!     1. `serde_json` round-trips every `vm-core` type without
//!        forcing the workspace to depend on `bincode` or `prost`.
//!     2. Frames are inspectable with `xxd | jq` during incident
//!        triage. Microbenchmarks show JSON adds ~3 µs vs `bincode`
//!        on a typical [`Request`]; we'd revisit at >100k req/s, not
//!        before.
//!
//! - **Unified tagged enums.** [`Request`] and [`Response`] both use
//!   `#[serde(tag = "kind")]` so the wire looks like
//!   `{"kind":"start","id":...}` rather than the externally-tagged
//!   `{"start":{"id":...}}` default. Tag-only adjacency is easier to
//!   grep in a captured pcap.
//!
//! - **One-shot reply per request.** No multiplexing, no streaming
//!   yet. Streaming exec arrives in a later milestone via a separate
//!   channel; the synchronous request/response shape here covers
//!   every lifecycle operation the orchestrator needs.
//!
//! # Roadmap
//!
//! This crate is shipped on its own, ahead of the consumers, so the
//! protocol can be reviewed in isolation. The follow-up PRs:
//!
//! - **PR-2: `nanovm-vmm-child` binary** — a single-VM worker that
//!   reads `Request`s on a Unix socket and dispatches to a
//!   `vm_mock::MockHypervisor` (and later a single-VM
//!   `vm_kvm::KvmHypervisor`).
//!
//! - **PR-3: `nanovm-jailer` + per-VM cgroup wiring** — a privileged
//!   helper that creates the per-VM cgroup, writes `memory.max` /
//!   `cpu.max`, then `execve()`s into `nanovm-vmm-child`.
//!
//! - **PR-4: process-fleet `Hypervisor` impl** in the control plane,
//!   gated behind a `process-isolation` feature flag.
//!
//! - **PR-5: pre-warmed VMM-process pool** to keep the headline cold
//!   start in the ~12 ms range.
//!
//! - **PR-6: flip the default and delete the in-process path.**

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};
use vm_core::{
    GuestExecRequest, GuestExecResult, SnapshotId, SnapshotMeta, VmConfig, VmHandle, VmId, VmMeta,
    VmState,
};

pub mod framing;

/// Default frame-length cap. 4 MiB easily covers a `WriteFile` with a
/// ~3 MiB payload (the largest realistic single-shot file the agent
/// API exposes today) plus the JSON envelope overhead. Bigger writes
/// are an explicit signal to use a streaming RPC, not a single
/// `write_file` request.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// One control-plane → child request. Every variant maps to one
/// method on the [`vm_core::Hypervisor`] trait (or to a child-only
/// lifecycle action like [`Request::Shutdown`]). The child guarantees
/// at most one in-flight request per connection — pipelining is a
/// later optimization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// Liveness check. The child replies with [`Response::Pong`]. Used
    /// by the orchestrator's startup handshake to wait for the
    /// socket to become readable before sending real traffic.
    Ping,
    /// Create the VM this worker owns. Workers are single-VM, so this
    /// is called at most once per worker lifetime; a second
    /// `CreateVm` after a destroy is an error.
    CreateVm {
        /// The VM's geometry. See [`vm_core::VmConfig`].
        config: VmConfig,
    },
    /// Transition this worker's VM into `Running`.
    Start {
        /// The VM id assigned at create time. Echoed back so a
        /// confused orchestrator can never start the wrong VM by
        /// accident.
        id: VmId,
    },
    /// Transition this worker's VM into `Stopped`.
    Stop {
        /// The VM id.
        id: VmId,
    },
    /// Capture an in-memory snapshot of this worker's VM.
    Snapshot {
        /// The VM id.
        id: VmId,
    },
    /// Restore a fresh VM from a captured snapshot. Used by the
    /// "fork from snapshot" path; the worker drops any previous
    /// in-process VM state and rebuilds from the snapshot bytes.
    Restore {
        /// The snapshot id, previously returned by [`Request::Snapshot`].
        id: SnapshotId,
    },
    /// Destroy this worker's VM. After this the worker is expected to
    /// either accept a fresh [`Request::CreateVm`] or exit on
    /// [`Request::Shutdown`].
    Destroy {
        /// The VM id.
        id: VmId,
    },
    /// Read this VM's lifecycle state.
    State {
        /// The VM id.
        id: VmId,
    },
    /// Read this VM's geometry + state in one round-trip.
    VmMeta {
        /// The VM id.
        id: VmId,
    },
    /// Read a captured snapshot's metadata.
    SnapshotMeta {
        /// The snapshot id.
        id: SnapshotId,
    },
    /// Delete a captured snapshot.
    DeleteSnapshot {
        /// The snapshot id.
        id: SnapshotId,
    },
    /// List the snapshots owned by this worker. (Workers own their
    /// snapshots; the orchestrator stitches the per-worker lists into
    /// a global view.)
    ListSnapshots,
    /// Run a program inside the guest. See
    /// [`vm_core::GuestExecRequest`] for the shape.
    ExecInGuest {
        /// The VM id.
        id: VmId,
        /// The exec parameters.
        req: GuestExecRequest,
    },
    /// Read a file out of the guest filesystem.
    ReadFile {
        /// The VM id.
        id: VmId,
        /// Absolute path inside the guest.
        path: String,
    },
    /// Write a file into the guest filesystem.
    WriteFile {
        /// The VM id.
        id: VmId,
        /// Absolute path inside the guest.
        path: String,
        /// File body.
        content: Vec<u8>,
        /// POSIX permission bits. Ignored on non-Unix hosts.
        mode: u32,
    },
    /// Cooperative shutdown. The child responds with
    /// [`Response::Empty`] and then closes the socket and exits. A
    /// killed worker (SIGKILL on cgroup OOM, etc.) is the
    /// uncooperative path the orchestrator must also handle.
    Shutdown,
    /// Ask the worker for an on-disk directory that holds the
    /// captured snapshot bytes. The orchestrator uses it for
    /// cross-worker `restore`: it asks the owner worker to export,
    /// then asks a *different* worker to [`Request::SnapshotAdopt`]
    /// the same directory. Backends that don't expose snapshot
    /// state on disk (mock, today's KVM) return
    /// [`Response::OptPath`] with `path: None`; the fleet then
    /// surfaces a clear "cross-worker restore unsupported on this
    /// backend" error instead of silently producing an empty VM.
    SnapshotExportDir {
        /// The snapshot id.
        id: SnapshotId,
    },
    /// Adopt a snapshot from a directory previously produced by
    /// [`Request::SnapshotExportDir`] on another worker (or out-
    /// of-band via the durable store). Returns a fresh
    /// [`SnapshotId`] in the adopting worker's local id space.
    SnapshotAdopt {
        /// Absolute path to the snapshot directory. Same shape
        /// the durable snapshot store dumps locally on `get`.
        dir: std::path::PathBuf,
    },
}

/// One child → control-plane response. Each variant pairs with one
/// or more [`Request`] kinds. The pairing is documented per-variant
/// rather than encoded in the type system so a future request can
/// reuse an existing response shape without churning the public API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Reply to [`Request::Ping`].
    Pong,
    /// Reply to mutators that don't produce a payload
    /// ([`Request::Start`], [`Request::Stop`], [`Request::Destroy`],
    /// [`Request::DeleteSnapshot`], [`Request::Shutdown`]).
    Empty,
    /// Reply to [`Request::CreateVm`] and [`Request::Restore`].
    VmHandle(VmHandle),
    /// Reply to [`Request::State`]. Wrapped in a named field because
    /// `#[serde(tag = "kind")]` can't merge a bare string/enum into
    /// the discriminator object.
    State {
        /// The VM's lifecycle state.
        state: VmState,
    },
    /// Reply to [`Request::VmMeta`].
    VmMeta(VmMeta),
    /// Reply to [`Request::Snapshot`].
    Snapshot {
        /// Newly-issued snapshot id.
        id: SnapshotId,
    },
    /// Reply to [`Request::SnapshotMeta`].
    SnapshotMeta(SnapshotMeta),
    /// Reply to [`Request::ListSnapshots`].
    SnapshotIds {
        /// Snapshot ids this worker owns.
        ids: Vec<SnapshotId>,
    },
    /// Reply to [`Request::ExecInGuest`].
    ExecResult(GuestExecResult),
    /// Reply to [`Request::ReadFile`].
    Bytes {
        /// File body.
        content: Vec<u8>,
    },
    /// Reply to [`Request::WriteFile`]. Carries the number of bytes
    /// actually written so the orchestrator can detect partial writes
    /// (which should never happen, but the signal is cheap).
    Written {
        /// Bytes written.
        bytes: u64,
    },
    /// Reply to [`Request::SnapshotExportDir`]. `None` means the
    /// backend doesn't expose snapshot state on disk for this id
    /// — the orchestrator should surface a "cross-worker restore
    /// unsupported" error rather than trying to adopt nothing.
    OptPath {
        /// Absolute path to the snapshot directory, or `None` if
        /// this backend can't export it.
        path: Option<std::path::PathBuf>,
    },
    /// Any operation that failed. The `code` field is stable
    /// machine-readable; `message` is human-readable and free to
    /// drift release-to-release.
    Error {
        /// Machine-readable code, mirroring the
        /// [`vm_core::VmError`] discriminant so the orchestrator can
        /// rebuild the typed error on the host side.
        code: ErrorCode,
        /// Human-readable detail.
        message: String,
    },
}

/// Stable machine-readable error tags. One-for-one with the
/// [`vm_core::VmError`] discriminants we care about on the wire.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// VM doesn't exist for this worker.
    UnknownVm,
    /// Snapshot doesn't exist for this worker.
    UnknownSnapshot,
    /// State-machine transition was invalid (e.g. start a Created VM
    /// that's already Running).
    InvalidTransition,
    /// Backend (KVM, mock, host syscall) failure. Catch-all for
    /// errors the orchestrator can't classify more specifically.
    Backend,
    /// The worker doesn't implement the requested capability. Used by
    /// stub workers + non-Linux test hosts.
    Unsupported,
    /// The request was malformed at the IPC layer (e.g. wrong VM id
    /// for this worker's single-owned VM, framing error, JSON shape
    /// mismatch).
    BadRequest,
}

impl Response {
    /// Convenience: build an [`Response::Error`].
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            code,
            message: message.into(),
        }
    }

    /// True when this is an [`Response::Error`]. Useful for the
    /// orchestrator's metric counters and tracing-span tagging.
    pub fn is_error(&self) -> bool {
        matches!(self, Response::Error { .. })
    }
}

/// Map a [`vm_core::VmError`] to the corresponding [`Response::Error`].
/// Used by the worker to translate its internal hypervisor error type
/// into the wire shape; lives here so the host and the worker agree
/// on the mapping by construction.
impl From<&vm_core::VmError> for Response {
    fn from(err: &vm_core::VmError) -> Self {
        use vm_core::VmError;
        let code = match err {
            VmError::UnknownVm(_) => ErrorCode::UnknownVm,
            VmError::UnknownSnapshot(_) => ErrorCode::UnknownSnapshot,
            VmError::InvalidTransition { .. } => ErrorCode::InvalidTransition,
            VmError::Unsupported(_) => ErrorCode::Unsupported,
            _ => ErrorCode::Backend,
        };
        Response::error(code, err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_config() -> VmConfig {
        VmConfig {
            vcpus: 2,
            memory_mib: 256,
            kernel: Some(PathBuf::from("/boot/vmlinuz")),
            flat_binary: None,
            initrd: None,
            rootfs: None,
            cmdline: "console=ttyS0".to_owned(),
            vsock_cid: Some(3),
            snapshot_dir: None,
        }
    }

    #[test]
    fn request_create_vm_round_trips_through_json() {
        let req = Request::CreateVm {
            config: sample_config(),
        };
        let json = serde_json::to_string(&req).unwrap();
        // Tag-only adjacency: the discriminant lives on the same
        // level as the per-variant fields.
        assert!(json.contains(r#""kind":"create_vm""#));
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn request_lifecycle_round_trips() {
        for req in [
            Request::Ping,
            Request::Start { id: VmId(42) },
            Request::Stop { id: VmId(42) },
            Request::Snapshot { id: VmId(42) },
            Request::Restore { id: SnapshotId(7) },
            Request::Destroy { id: VmId(42) },
            Request::State { id: VmId(42) },
            Request::Shutdown,
        ] {
            let json = serde_json::to_string(&req).unwrap();
            let back: Request = serde_json::from_str(&json).unwrap();
            assert_eq!(back, req);
        }
    }

    #[test]
    fn request_file_ops_round_trip() {
        let w = Request::WriteFile {
            id: VmId(1),
            path: "/etc/x".into(),
            content: b"hello".to_vec(),
            mode: 0o644,
        };
        let json = serde_json::to_string(&w).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back, w);

        let r = Request::ReadFile {
            id: VmId(1),
            path: "/etc/x".into(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn response_variants_round_trip() {
        let cases = [
            Response::Pong,
            Response::Empty,
            Response::VmHandle(VmHandle {
                id: VmId(1),
                state: VmState::Running,
            }),
            Response::State {
                state: VmState::Created,
            },
            Response::Snapshot { id: SnapshotId(9) },
            Response::SnapshotIds {
                ids: vec![SnapshotId(1), SnapshotId(2)],
            },
            Response::Bytes {
                content: b"abc".to_vec(),
            },
            Response::Written { bytes: 123 },
            Response::Error {
                code: ErrorCode::UnknownVm,
                message: "no such vm".into(),
            },
        ];
        for r in cases {
            let json = serde_json::to_string(&r).unwrap();
            let back: Response = serde_json::from_str(&json).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn from_vm_error_maps_known_variants_to_stable_codes() {
        let r: Response = (&vm_core::VmError::UnknownVm(VmId(7))).into();
        assert!(matches!(
            r,
            Response::Error {
                code: ErrorCode::UnknownVm,
                ..
            }
        ));

        let r: Response = (&vm_core::VmError::UnknownSnapshot(SnapshotId(7))).into();
        assert!(matches!(
            r,
            Response::Error {
                code: ErrorCode::UnknownSnapshot,
                ..
            }
        ));

        let r: Response = (&vm_core::VmError::InvalidTransition {
            id: VmId(7),
            from: VmState::Created,
            to: VmState::Running,
        })
            .into();
        assert!(matches!(
            r,
            Response::Error {
                code: ErrorCode::InvalidTransition,
                ..
            }
        ));

        let r: Response = (&vm_core::VmError::Unsupported("nope")).into();
        assert!(matches!(
            r,
            Response::Error {
                code: ErrorCode::Unsupported,
                ..
            }
        ));

        let r: Response = (&vm_core::VmError::Backend("kvm boom".into())).into();
        assert!(matches!(
            r,
            Response::Error {
                code: ErrorCode::Backend,
                ..
            }
        ));
    }

    #[test]
    fn error_code_serializes_as_snake_case() {
        let s = serde_json::to_string(&ErrorCode::UnknownVm).unwrap();
        assert_eq!(s, r#""unknown_vm""#);
        let s = serde_json::to_string(&ErrorCode::InvalidTransition).unwrap();
        assert_eq!(s, r#""invalid_transition""#);
    }

    #[test]
    fn response_is_error_is_correct() {
        assert!(Response::error(ErrorCode::Backend, "x").is_error());
        assert!(!Response::Pong.is_error());
        assert!(!Response::Empty.is_error());
    }
}
