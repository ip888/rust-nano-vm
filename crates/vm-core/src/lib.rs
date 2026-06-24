//! Core traits and types for `rust-nano-vm`.
//!
//! Every hypervisor backend (`vm-kvm`, `vm-mock`, future in-process or
//! userfaultfd-based backends) implements the [`Hypervisor`] trait. Consumers
//! (CLI, control-plane) program against the trait, never the concrete
//! backend, so test and CI runs can use `vm-mock` without a KVM device.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

/// Opaque identifier for a VM instance managed by a [`Hypervisor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct VmId(pub u64);

impl fmt::Display for VmId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "vm-{:016x}", self.0)
    }
}

impl VmId {
    /// Allocate a fresh, process-unique [`VmId`].
    ///
    /// The counter is monotonic within a process; it is not stable across
    /// restarts and must not be persisted as a long-term identifier.
    pub fn next() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

/// Opaque identifier for a snapshot captured from a running VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SnapshotId(pub u64);

impl fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "snap-{:016x}", self.0)
    }
}

impl SnapshotId {
    /// Allocate a fresh, process-unique [`SnapshotId`].
    pub fn next() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

/// Configuration for a new VM.
///
/// `snapshot_dir`, when set, asks the backend to restore the VM from a
/// previously-captured snapshot directory (see the `snapshot` crate's
/// `Manifest` / `BackingFileHeader`) instead of cold-booting from
/// `kernel` / `rootfs`. Backends that don't support snapshot restore
/// return [`VmError::Unsupported`]; backends that do should treat the
/// manifest's `memory_bytes` / `vcpu_count` as authoritative when
/// `snapshot_dir` is set, ignoring the matching fields in this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct VmConfig {
    /// Number of virtual CPUs. Ignored when [`Self::snapshot_dir`] is set
    /// (the snapshot's `vcpu_count` wins).
    pub vcpus: u32,
    /// Guest memory in MiB. Ignored when [`Self::snapshot_dir`] is set
    /// (the snapshot's `memory_bytes` wins).
    pub memory_mib: u64,
    /// Path to the kernel image (bzImage or vmlinux). `None` is allowed for
    /// backends that don't actually boot a guest (e.g. `vm-mock`), and
    /// also when [`Self::snapshot_dir`] is set (the snapshot supplies the
    /// kernel state).
    pub kernel: Option<PathBuf>,
    /// Raw bytes to load at guest physical address 0, executed in 16-bit
    /// real mode at `CS:IP = 0000:0000`. Intended for tiny hand-rolled
    /// test programs that exercise the KVM bring-up path without dragging
    /// in a full Linux kernel build — see
    /// `crates/vm-kvm/tests/flat_binary.rs` for the canonical use.
    ///
    /// Mutually exclusive with [`Self::kernel`]; backends that see both
    /// set return [`VmError::Backend`].
    pub flat_binary: Option<Vec<u8>>,
    /// Path to an initramfs/initrd image to hand the kernel at boot.
    /// The backend loads it into guest RAM and points the boot
    /// params' `ramdisk_image`/`ramdisk_size` at it. Only meaningful
    /// alongside [`Self::kernel`]; ignored for `flat_binary` and mock
    /// backends. `None` cold-boots with no initramfs (the kernel
    /// panics on no init unless a rootfs is supplied another way).
    pub initrd: Option<PathBuf>,
    /// Path to the root filesystem image.
    pub rootfs: Option<PathBuf>,
    /// Kernel command line to pass at boot. Empty is allowed for mocks.
    pub cmdline: String,
    /// vsock context id for host↔guest RPC, filled in during M2.
    pub vsock_cid: Option<u32>,
    /// Snapshot directory to restore from. When `Some`, the backend reads
    /// `manifest.json` + `memory.cow` from this directory and uses the
    /// captured state instead of booting fresh.
    pub snapshot_dir: Option<PathBuf>,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            vcpus: 1,
            memory_mib: 128,
            kernel: None,
            flat_binary: None,
            initrd: None,
            rootfs: None,
            cmdline: String::new(),
            vsock_cid: None,
            snapshot_dir: None,
        }
    }
}

/// Lifecycle state of a VM managed by a [`Hypervisor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum VmState {
    /// Allocated but not yet started.
    Created,
    /// vCPU threads are running.
    Running,
    /// Stopped; guest memory may still be resident for snapshotting.
    Stopped,
}

/// Handle returned by [`Hypervisor::create_vm`]. Carries identity + current
/// lifecycle state as observed at the moment the handle was issued or last
/// returned from an operation. Callers should not treat [`VmHandle::state`]
/// as authoritative over time — query the hypervisor for a fresh read.
#[derive(Debug, Clone)]
pub struct VmHandle {
    /// Unique id assigned by the hypervisor.
    pub id: VmId,
    /// Observed state at issue time.
    pub state: VmState,
}

/// Errors returned by [`Hypervisor`] implementations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum VmError {
    /// The requested operation is not supported on this backend/platform.
    #[error("operation not supported: {0}")]
    Unsupported(&'static str),

    /// Referenced [`VmId`] is not known to this hypervisor.
    #[error("unknown vm id: {0}")]
    UnknownVm(VmId),

    /// Referenced [`SnapshotId`] is not known to this hypervisor.
    #[error("unknown snapshot id: {0}")]
    UnknownSnapshot(SnapshotId),

    /// The VM is in a state incompatible with the requested operation.
    #[error("invalid state transition for {id}: {from:?} -> {to:?}")]
    InvalidTransition {
        /// Which VM rejected the transition.
        id: VmId,
        /// State observed at the time of the rejection.
        from: VmState,
        /// State the caller tried to reach.
        to: VmState,
    },

    /// Underlying OS/IO/KVM failure.
    #[error("backend error: {0}")]
    Backend(String),
}

/// Result alias for hypervisor operations.
pub type VmResult<T> = Result<T, VmError>;

/// Uniform interface every VM backend must expose.
///
/// Implementations must be `Send + Sync` so that a single hypervisor can be
/// shared across request handlers in the control plane.
pub trait Hypervisor: Send + Sync {
    /// Allocate but do not start a new VM.
    fn create_vm(&self, cfg: &VmConfig) -> VmResult<VmHandle>;

    /// Start a previously-created VM.
    fn start(&self, id: VmId) -> VmResult<()>;

    /// Gracefully stop a running VM.
    fn stop(&self, id: VmId) -> VmResult<()>;

    /// Report the current state of a VM.
    fn state(&self, id: VmId) -> VmResult<VmState>;

    /// Capture a snapshot of a VM. The VM must typically be `Stopped` or
    /// paused; implementations may reject `Running` states.
    fn snapshot(&self, id: VmId) -> VmResult<SnapshotId>;

    /// Restore a VM from a snapshot. Returns a new handle; the snapshot
    /// itself is reusable unless the backend documents otherwise.
    fn restore(&self, snap: SnapshotId) -> VmResult<VmHandle>;

    /// Destroy a VM and release its resources. After this returns `Ok`, the
    /// [`VmId`] must not be reused.
    fn destroy(&self, id: VmId) -> VmResult<()>;

    /// Enumerate every VM currently known to this hypervisor.
    ///
    /// Order is implementation-defined. Each returned [`VmHandle`] carries
    /// the VM's state at the moment the listing was assembled — that read
    /// is not synchronised with concurrent transitions, so a handle may
    /// report a stale state by the time the caller inspects it. Re-query
    /// [`Hypervisor::state`] for an authoritative read.
    ///
    /// Backends that don't track in-process state (e.g. wrappers around an
    /// out-of-band orchestrator) may return [`VmError::Unsupported`].
    fn list_vms(&self) -> VmResult<Vec<VmHandle>>;

    /// Enumerate every snapshot currently held by this hypervisor.
    ///
    /// Order is implementation-defined. Backends that don't retain
    /// in-process snapshot state may return [`VmError::Unsupported`].
    fn list_snapshots(&self) -> VmResult<Vec<SnapshotId>>;

    /// Drop a previously-captured snapshot. After this returns `Ok`, the
    /// [`SnapshotId`] is invalid and any subsequent
    /// [`Hypervisor::restore`] call referencing it returns
    /// [`VmError::UnknownSnapshot`].
    fn delete_snapshot(&self, snap: SnapshotId) -> VmResult<()>;

    /// Read the metadata recorded for a snapshot. The control plane uses
    /// this when persisting a snapshot directory to disk: it retrieves
    /// the geometry the backend captured and writes a `Manifest` from
    /// it. Backends that don't track in-process snapshot state may
    /// return [`VmError::Unsupported`].
    fn snapshot_meta(&self, snap: SnapshotId) -> VmResult<SnapshotMeta>;

    /// Read metadata describing a VM's geometry — what it was created
    /// with, plus its current observed state. The control plane uses
    /// this to enrich `GET /v1/vms` so operators see vcpu / memory
    /// columns without an extra round-trip per VM. Backends that
    /// don't track per-VM state in-process may return
    /// [`VmError::Unsupported`].
    fn vm_meta(&self, id: VmId) -> VmResult<VmMeta>;

    /// Execute a command in the guest and wait for it to finish.
    ///
    /// The VM must be in the `Running` state; callers that pass a
    /// non-running VM id receive [`VmError::InvalidTransition`].
    ///
    /// The default implementation returns [`VmError::Unsupported`].
    /// Backends that proxy the call to a real guest agent (M2+) or
    /// run it locally (mock) should override this.
    fn exec_in_guest(&self, _id: VmId, _req: GuestExecRequest) -> VmResult<GuestExecResult> {
        Err(VmError::Unsupported(
            "exec_in_guest: not implemented on this backend",
        ))
    }

    /// Execute a command in the guest and stream stdout/stderr/exit
    /// frames back as they're produced. Pulls one frame at a time via
    /// [`ExecStream::next_frame`].
    ///
    /// The VM must be in the `Running` state; callers that pass a
    /// non-running VM id receive [`VmError::InvalidTransition`].
    ///
    /// The default implementation returns [`VmError::Unsupported`].
    /// Backends that already implement the one-shot [`exec_in_guest`]
    /// can override this with a streaming variant — typically by
    /// pushing chunks into a channel from a worker thread.
    ///
    /// [`exec_in_guest`]: Hypervisor::exec_in_guest
    fn exec_in_guest_stream(
        &self,
        _id: VmId,
        _req: GuestExecRequest,
    ) -> VmResult<Box<dyn ExecStream>> {
        Err(VmError::Unsupported(
            "exec_in_guest_stream: not implemented on this backend",
        ))
    }

    /// Write a file into the guest filesystem. The VM must be `Running`.
    ///
    /// `path` is the absolute path inside the guest.  `mode` is the
    /// UNIX permission bits (e.g. `0o644`); ignored on non-Unix hosts.
    /// Returns the number of bytes written.
    ///
    /// The default implementation returns [`VmError::Unsupported`].
    fn write_file(&self, _id: VmId, _path: String, _content: Vec<u8>, _mode: u32) -> VmResult<u64> {
        Err(VmError::Unsupported(
            "write_file: not implemented on this backend",
        ))
    }

    /// Read a file from the guest filesystem. The VM must be `Running`.
    ///
    /// `path` is the absolute path inside the guest. Returns the raw
    /// file bytes.
    ///
    /// The default implementation returns [`VmError::Unsupported`].
    fn read_file(&self, _id: VmId, _path: String) -> VmResult<Vec<u8>> {
        Err(VmError::Unsupported(
            "read_file: not implemented on this backend",
        ))
    }
}

/// Metadata describing a VM's geometry. Read via
/// [`Hypervisor::vm_meta`]. State is the snapshot at the moment the
/// read assembled — same caveat as [`VmHandle::state`]: a concurrent
/// transition can stale it before the caller looks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmMeta {
    /// VM identifier.
    pub id: VmId,
    /// Lifecycle state at read time.
    pub state: VmState,
    /// vCPU count this VM was created with (or that the snapshot it
    /// was restored from carried).
    pub vcpus: u32,
    /// Guest memory in MiB the VM was created with.
    pub memory_mib: u64,
    /// Kernel command line the VM was given at create time.
    pub kernel_cmdline: String,
    /// Snapshot directory the VM was restored from, if any.
    pub snapshot_dir: Option<std::path::PathBuf>,
}

/// Metadata describing a captured snapshot. Read via
/// [`Hypervisor::snapshot_meta`]; mirrors the relevant fields of the
/// `snapshot::Manifest` produced when a snapshot is persisted to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotMeta {
    /// Snapshot identifier.
    pub id: SnapshotId,
    /// vCPU count captured at snapshot time.
    pub vcpu_count: u32,
    /// Guest memory size in bytes.
    pub memory_bytes: u64,
    /// Guest page size at snapshot time.
    pub page_size: u32,
    /// Kernel command line captured at snapshot time. Empty when the
    /// VM was never given one (typical for the mock backend).
    pub kernel_cmdline: String,
}

/// Request parameters for [`Hypervisor::exec_in_guest`].
#[derive(Debug, Clone)]
pub struct GuestExecRequest {
    /// Program to execute (absolute path or found on `$PATH`).
    pub program: String,
    /// Argument vector, NOT including `argv[0]`.
    pub args: Vec<String>,
    /// Optional working directory inside the guest (or on the host for
    /// the mock backend).
    pub cwd: Option<String>,
    /// Extra environment variables to inject into the child.
    pub env: Vec<(String, String)>,
    /// Wall-clock timeout in milliseconds. `None` means no limit.
    pub timeout_ms: Option<u64>,
}

/// Outcome of [`Hypervisor::exec_in_guest`].
#[derive(Debug, Clone)]
pub struct GuestExecResult {
    /// Process exit code. `None` if the process was killed by a signal.
    pub exit_code: Option<i32>,
    /// Signal that terminated the process, if any (POSIX only).
    pub signal: Option<i32>,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
    /// Wall-clock runtime in milliseconds.
    pub duration_ms: u64,
}

/// One unit of output from a streaming guest exec, produced by
/// [`Hypervisor::exec_in_guest_stream`].
///
/// A well-formed stream emits zero or more [`ExecFrame::Stdout`] /
/// [`ExecFrame::Stderr`] frames in arrival order, then exactly one
/// terminal [`ExecFrame::Exit`] frame, after which
/// [`ExecStream::next_frame`] returns `Ok(None)`. Backends are free to
/// coalesce or split chunk boundaries however the underlying transport
/// surfaces them — callers must not assume one frame per line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecFrame {
    /// Bytes from the guest process's stdout.
    Stdout(Vec<u8>),
    /// Bytes from the guest process's stderr.
    Stderr(Vec<u8>),
    /// Terminal frame — process finished. At most one per stream.
    Exit {
        /// Process exit code. `None` if killed by a signal.
        exit_code: Option<i32>,
        /// Signal that terminated the process, if any (POSIX only).
        signal: Option<i32>,
        /// Wall-clock runtime in milliseconds.
        duration_ms: u64,
    },
}

/// Pull-iterator over [`ExecFrame`]s produced by a streaming guest
/// exec. Each call to [`next_frame`](ExecStream::next_frame) blocks
/// until the next frame is available (or the stream ends). Returning
/// `Ok(None)` signals end-of-stream; callers should drop the stream
/// at that point.
///
/// The trait is `Send` so the control plane can move the stream onto
/// a `spawn_blocking` worker that pushes frames into an async channel.
/// The `Debug` bound keeps `Result::unwrap_err`-style assertions
/// usable in tests without callers having to match by hand.
pub trait ExecStream: Send + std::fmt::Debug {
    /// Block until the next frame is available, or `Ok(None)` if the
    /// stream has ended.
    fn next_frame(&mut self) -> VmResult<Option<ExecFrame>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_id_allocates_unique_monotonic_ids() {
        let a = VmId::next();
        let b = VmId::next();
        assert_ne!(a, b);
        assert!(b.0 > a.0);
    }

    #[test]
    fn snapshot_id_allocates_unique_monotonic_ids() {
        let a = SnapshotId::next();
        let b = SnapshotId::next();
        assert_ne!(a, b);
        assert!(b.0 > a.0);
    }

    #[test]
    fn vm_id_display_is_stable() {
        assert_eq!(VmId(0x42).to_string(), "vm-0000000000000042");
        assert_eq!(SnapshotId(0x1).to_string(), "snap-0000000000000001");
    }

    #[test]
    fn vm_config_default_is_minimal_and_boot_less() {
        let cfg = VmConfig::default();
        assert_eq!(cfg.vcpus, 1);
        assert_eq!(cfg.memory_mib, 128);
        assert!(cfg.kernel.is_none());
        assert!(cfg.rootfs.is_none());
        assert!(cfg.cmdline.is_empty());
        assert!(cfg.vsock_cid.is_none());
        assert!(cfg.snapshot_dir.is_none());
    }

    #[test]
    fn vm_error_invalid_transition_renders() {
        let err = VmError::InvalidTransition {
            id: VmId(1),
            from: VmState::Stopped,
            to: VmState::Running,
        };
        let msg = err.to_string();
        assert!(msg.contains("vm-"));
        assert!(msg.contains("Stopped"));
        assert!(msg.contains("Running"));
    }
}
