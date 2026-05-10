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

use vm_core::{
    GuestExecRequest, GuestExecResult, Hypervisor, SnapshotId, SnapshotMeta, VmConfig, VmError,
    VmHandle, VmId, VmMeta, VmResult, VmState,
};

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

    fn list_snapshots(&self) -> VmResult<Vec<SnapshotId>> {
        Err(unsupported())
    }

    fn delete_snapshot(&self, _snap: SnapshotId) -> VmResult<()> {
        Err(unsupported())
    }

    fn snapshot_meta(&self, _snap: SnapshotId) -> VmResult<SnapshotMeta> {
        Err(unsupported())
    }

    fn vm_meta(&self, _id: VmId) -> VmResult<VmMeta> {
        Err(unsupported())
    }

    fn exec_in_guest(&self, _id: VmId, _req: GuestExecRequest) -> VmResult<GuestExecResult> {
        Err(unsupported())
    }

    fn write_file(&self, _id: VmId, _path: String, _content: Vec<u8>, _mode: u32) -> VmResult<u64> {
        Err(unsupported())
    }

    fn read_file(&self, _id: VmId, _path: String) -> VmResult<Vec<u8>> {
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
        assert!(matches!(
            hv.list_snapshots().unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.delete_snapshot(SnapshotId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.snapshot_meta(SnapshotId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.vm_meta(VmId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.exec_in_guest(
                VmId(1),
                GuestExecRequest {
                    program: "echo".into(),
                    args: vec![],
                    cwd: None,
                    env: vec![],
                    timeout_ms: None,
                }
            )
            .unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.write_file(VmId(1), "/tmp/x".into(), vec![], 0o644)
                .unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.read_file(VmId(1), "/tmp/x".into()).unwrap_err(),
            VmError::Unsupported(_)
        ));
    }
}
