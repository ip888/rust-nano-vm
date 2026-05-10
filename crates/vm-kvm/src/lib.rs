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

#[cfg(feature = "kvm")]
use kvm_ioctls::Kvm;
#[cfg(feature = "kvm")]
use linux_loader::cmdline::Cmdline;
#[cfg(feature = "kvm")]
use vm_memory::{Address, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

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

    /// Build a minimal, compile-time-validated boot plan without touching
    /// `/dev/kvm`.
    ///
    /// This is the first M1 slice we can land in a non-KVM sandbox: it proves
    /// the feature-gated dependencies compile together and that we can derive
    /// guest memory / command-line structures from [`VmConfig`].
    #[cfg(feature = "kvm")]
    #[allow(dead_code)]
    pub(crate) fn boot_plan(cfg: &VmConfig) -> VmResult<KvmBootPlan> {
        KvmBootPlan::from_config(cfg)
    }
}

/// Minimal KVM boot resources that can be prepared without creating a VM.
#[cfg(feature = "kvm")]
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct KvmBootPlan {
    guest_mem: GuestMemoryMmap,
    kernel_load_addr: GuestAddress,
    initrd_load_addr: Option<GuestAddress>,
    cmdline: Cmdline,
}

#[cfg(feature = "kvm")]
#[allow(dead_code)]
impl KvmBootPlan {
    const CMDLINE_CAPACITY: usize = 4096;
    const KERNEL_LOAD_ADDR: u64 = 0x20_0000;

    fn from_config(cfg: &VmConfig) -> VmResult<Self> {
        let mem_size = cfg
            .memory_mib
            .checked_mul(1024 * 1024)
            .ok_or_else(|| VmError::Backend("memory size overflow".into()))?;
        let mem_len = usize::try_from(mem_size)
            .map_err(|_| VmError::Backend(format!("memory size {mem_size} does not fit usize")))?;
        let guest_mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_len)])
            .map_err(|e| VmError::Backend(format!("guest memory layout: {e}")))?;
        let mut cmdline = Cmdline::new(Self::CMDLINE_CAPACITY)
            .map_err(|e| VmError::Backend(format!("kernel cmdline capacity: {e}")))?;
        if !cfg.cmdline.is_empty() {
            cmdline
                .insert_str(&cfg.cmdline)
                .map_err(|e| VmError::Backend(format!("kernel cmdline: {e}")))?;
        }
        Ok(Self {
            guest_mem,
            kernel_load_addr: GuestAddress(Self::KERNEL_LOAD_ADDR),
            initrd_load_addr: None,
            cmdline,
        })
    }

    fn memory_size_bytes(&self) -> u64 {
        self.guest_mem.last_addr().raw_value() + 1
    }

    fn open_kvm() -> VmResult<Kvm> {
        Kvm::new().map_err(|e| VmError::Backend(format!("open /dev/kvm: {e}")))
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

    #[cfg(feature = "kvm")]
    #[test]
    fn boot_plan_derives_memory_and_cmdline_without_opening_kvm() {
        let plan = KvmHypervisor::boot_plan(&VmConfig {
            memory_mib: 64,
            cmdline: "console=ttyS0".into(),
            ..VmConfig::default()
        })
        .expect("boot plan");
        assert_eq!(plan.memory_size_bytes(), 64 * 1024 * 1024);
        assert_eq!(plan.kernel_load_addr, GuestAddress(0x20_0000));
        assert!(plan.initrd_load_addr.is_none());
    }
}
