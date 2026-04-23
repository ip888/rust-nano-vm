//! KVM-backed [`Hypervisor`] implementation.
//!
//! Scope: M0 ships only the skeleton. Real boot (kernel load, vCPU run,
//! serial console) arrives in M1 behind the `kvm` feature flag. Keeping the
//! heavy dependencies (`kvm-ioctls`, `vm-memory`, `linux-loader`) off the
//! default build ensures `cargo build --workspace` stays portable — in
//! particular this crate compiles cleanly in the gVisor sandbox that has no
//! `/dev/kvm`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use vm_core::{Hypervisor, SnapshotId, VmConfig, VmError, VmHandle, VmId, VmResult, VmState};

/// KVM-backed hypervisor.
///
/// On platforms without `/dev/kvm` (non-Linux or the `kvm` feature disabled)
/// every method returns [`VmError::Unsupported`]. This lets the rest of the
/// workspace depend on the type unconditionally.
#[derive(Debug, Default)]
pub struct KvmHypervisor {
    _private: (),
}

impl KvmHypervisor {
    /// Construct a new KVM hypervisor handle.
    ///
    /// In M0 this is infallible because no device is actually opened. Once
    /// the `kvm` feature lands in M1, this will return `VmResult<Self>` and
    /// open `/dev/kvm`.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Hypervisor for KvmHypervisor {
    fn create_vm(&self, _cfg: &VmConfig) -> VmResult<VmHandle> {
        Err(unsupported())
    }

    fn start(&self, _id: VmId) -> VmResult<()> {
        Err(unsupported())
    }

    fn stop(&self, _id: VmId) -> VmResult<()> {
        Err(unsupported())
    }

    fn state(&self, _id: VmId) -> VmResult<VmState> {
        Err(unsupported())
    }

    fn snapshot(&self, _id: VmId) -> VmResult<SnapshotId> {
        Err(unsupported())
    }

    fn restore(&self, _snap: SnapshotId) -> VmResult<VmHandle> {
        Err(unsupported())
    }

    fn destroy(&self, _id: VmId) -> VmResult<()> {
        Err(unsupported())
    }

    fn list_vms(&self) -> VmResult<Vec<VmHandle>> {
        Err(unsupported())
    }
}

#[cfg(feature = "kvm")]
fn unsupported() -> VmError {
    // Once M1 lands, this path will hold the real implementation and only
    // the non-`kvm` stub below will return Unsupported.
    VmError::Unsupported("vm-kvm: M1 implementation not yet landed")
}

#[cfg(not(feature = "kvm"))]
fn unsupported() -> VmError {
    VmError::Unsupported("vm-kvm: build without the `kvm` feature")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_methods_return_unsupported_in_m0() {
        let hv = KvmHypervisor::new();
        assert!(matches!(
            hv.create_vm(&VmConfig::default()).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.start(VmId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.stop(VmId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.state(VmId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.snapshot(VmId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.restore(SnapshotId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.destroy(VmId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.list_vms().unwrap_err(),
            VmError::Unsupported(_)
        ));
    }
}
