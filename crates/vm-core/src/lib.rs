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

/// Configuration for a new VM. Minimal M0 surface; will grow with M1/M2/M5.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct VmConfig {
    /// Number of virtual CPUs.
    pub vcpus: u32,
    /// Guest memory in MiB.
    pub memory_mib: u64,
    /// Path to the kernel image (bzImage or vmlinux). `None` is allowed for
    /// backends that don't actually boot a guest (e.g. `vm-mock`).
    pub kernel: Option<PathBuf>,
    /// Path to the root filesystem image.
    pub rootfs: Option<PathBuf>,
    /// Kernel command line to pass at boot. Empty is allowed for mocks.
    pub cmdline: String,
    /// vsock context id for host↔guest RPC, filled in during M2.
    pub vsock_cid: Option<u32>,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            vcpus: 1,
            memory_mib: 128,
            kernel: None,
            rootfs: None,
            cmdline: String::new(),
            vsock_cid: None,
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
