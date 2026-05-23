//! KVM-backed [`Hypervisor`] implementation.
//!
//! Scope: M0 ships only the skeleton. Real boot (kernel load, vCPU run,
//! serial console) arrives in M1 behind the `kvm` feature flag. Keeping the
//! heavy dependencies (`kvm-ioctls`, `vm-memory`, `linux-loader`) off the
//! default build ensures `cargo build --workspace` stays portable — in
//! particular this crate compiles cleanly in the gVisor sandbox that has no
//! `/dev/kvm`.
//!
//! `vm-kvm` now contains a small amount of required `unsafe` code for the
//! KVM userspace ABI: registering guest RAM requires passing a host virtual
//! address to `KVM_SET_USER_MEMORY_REGION`, and the safety invariants are
//! documented at each call site.

#![warn(missing_docs)]

use vm_core::{
    GuestExecRequest, GuestExecResult, Hypervisor, SnapshotId, SnapshotMeta, VmConfig, VmError,
    VmHandle, VmId, VmMeta, VmResult, VmState,
};

#[cfg(feature = "kvm")]
use std::collections::HashMap;
#[cfg(feature = "kvm")]
use std::fs::File;
#[cfg(feature = "kvm")]
use std::io::{self, Write};
#[cfg(feature = "kvm")]
use std::mem;
#[cfg(feature = "kvm")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "kvm")]
use std::sync::{Arc, Mutex, OnceLock};
#[cfg(feature = "kvm")]
use std::thread::{self, JoinHandle};

#[cfg(feature = "kvm")]
use kvm_bindings::{
    kvm_fpu, kvm_pit_config, kvm_regs, kvm_segment, kvm_sregs, kvm_userspace_memory_region,
    KVM_MAX_CPUID_ENTRIES, KVM_PIT_SPEAKER_DUMMY,
};
#[cfg(feature = "kvm")]
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
#[cfg(feature = "kvm")]
use libc::{c_int, c_void, siginfo_t, EINTR};
#[cfg(feature = "kvm")]
use linux_loader::cmdline::Cmdline;
#[cfg(feature = "kvm")]
use linux_loader::configurator::linux::LinuxBootConfigurator;
#[cfg(feature = "kvm")]
use linux_loader::configurator::{BootConfigurator, BootParams};
#[cfg(feature = "kvm")]
use linux_loader::loader::bootparam::{boot_params, setup_header};
#[cfg(feature = "kvm")]
use linux_loader::loader::{load_cmdline, BzImage, KernelLoader};
#[cfg(feature = "kvm")]
use vm_memory::{
    Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion,
    MemoryRegionAddress,
};
#[cfg(feature = "kvm")]
use vmm_sys_util::signal::{register_signal_handler, Killable};

/// KVM-backed hypervisor.
///
/// On platforms without `/dev/kvm` (non-Linux or the `kvm` feature disabled)
/// every method returns [`VmError::Unsupported`]. This lets the rest of the
/// workspace depend on the type unconditionally.
#[cfg(feature = "kvm")]
#[derive(Debug)]
pub struct KvmHypervisor {
    kvm: Kvm,
    inner: Mutex<Inner>,
    kick_signal: c_int,
}

/// KVM-backed hypervisor stub when built without the `kvm` feature.
#[cfg(not(feature = "kvm"))]
#[derive(Debug, Default)]
pub struct KvmHypervisor {
    _private: (),
}

#[cfg(feature = "kvm")]
#[derive(Debug, Default)]
struct Inner {
    vms: HashMap<VmId, KvmVm>,
}

#[cfg(feature = "kvm")]
#[derive(Debug)]
struct KvmVm {
    config: VmConfig,
    state: VmState,
    runtime: KvmVmRuntime,
    vcpu: Option<KvmVcpuThread>,
    last_run_error: Option<String>,
}

#[cfg(feature = "kvm")]
#[derive(Debug)]
struct KvmVmRuntime {
    vm_fd: VmFd,
    boot_plan: KvmBootPlan,
    entry_point: GuestAddress,
    serial_output: Arc<Mutex<Vec<u8>>>,
    /// When `true`, the vCPU starts in 16-bit real mode at
    /// `CS:IP = 0000:0000`. Used by [`VmConfig::flat_binary`].
    /// `false` selects the existing protected-mode Linux boot path.
    real_mode: bool,
}

#[cfg(feature = "kvm")]
#[derive(Debug)]
struct KvmVcpuThread {
    stop_requested: Arc<AtomicBool>,
    handle: JoinHandle<VmResult<()>>,
}

impl KvmHypervisor {
    /// Construct a new KVM hypervisor handle.
    #[cfg(feature = "kvm")]
    pub fn new() -> VmResult<Self> {
        Ok(Self {
            kvm: KvmBootPlan::open_kvm()?,
            inner: Mutex::new(Inner::default()),
            kick_signal: vcpu_kick_signal()?,
        })
    }

    /// Construct a new KVM hypervisor stub handle.
    #[cfg(not(feature = "kvm"))]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the bytes the guest has emitted to the serial port
    /// (COM1, `0x3f8`) so far. Returns an empty `Vec` if the VM has
    /// not written anything; returns [`VmError::UnknownVm`] if `id`
    /// is not tracked. Used by tests / integration harnesses that
    /// need to assert what the guest produced.
    #[cfg(feature = "kvm")]
    pub fn serial_output(&self, id: VmId) -> VmResult<Vec<u8>> {
        let inner = self.lock_inner()?;
        let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
        let buf = vm
            .runtime
            .serial_output
            .lock()
            .map_err(|_| VmError::Backend("vm-kvm: serial output mutex poisoned".into()))?;
        Ok(buf.clone())
    }

    /// Stub for non-KVM builds. Always returns `Unsupported`.
    #[cfg(not(feature = "kvm"))]
    pub fn serial_output(&self, _id: VmId) -> VmResult<Vec<u8>> {
        Err(VmError::Unsupported(
            "vm-kvm: serial_output requires the `kvm` feature",
        ))
    }

    /// The reason the VM's vCPU thread last terminated, if it has.
    /// `None` while still running or never started; `Some("")` after
    /// a clean `HLT`; `Some(msg)` carrying the diagnostic (e.g. a
    /// triple-fault register dump) when the vCPU stopped abnormally.
    /// Used by tests / triage to see *why* a guest stopped.
    #[cfg(feature = "kvm")]
    pub fn last_run_error(&self, id: VmId) -> VmResult<Option<String>> {
        let inner = self.lock_inner()?;
        let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
        Ok(vm.last_run_error.clone())
    }

    /// Stub for non-KVM builds. Always returns `Unsupported`.
    #[cfg(not(feature = "kvm"))]
    pub fn last_run_error(&self, _id: VmId) -> VmResult<Option<String>> {
        Err(VmError::Unsupported(
            "vm-kvm: last_run_error requires the `kvm` feature",
        ))
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

#[cfg(feature = "kvm")]
impl KvmHypervisor {
    fn lock_inner(&self) -> VmResult<std::sync::MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| VmError::Backend("vm-kvm: hypervisor state mutex poisoned".into()))
    }

    fn build_runtime(&self, cfg: &VmConfig) -> VmResult<KvmVmRuntime> {
        if cfg.snapshot_dir.is_some() {
            return Err(VmError::Unsupported(
                "vm-kvm: snapshot restore is not implemented in M1",
            ));
        }
        if cfg.vcpus != 1 {
            return Err(VmError::Unsupported(
                "vm-kvm: M1 currently supports exactly one vCPU",
            ));
        }
        if cfg.flat_binary.is_some() && cfg.kernel.is_some() {
            return Err(VmError::Backend(
                "vm-kvm: VmConfig.kernel and VmConfig.flat_binary are mutually exclusive".into(),
            ));
        }
        if let Some(bytes) = cfg.flat_binary.as_ref() {
            return self.build_flat_runtime(cfg, bytes);
        }
        let kernel = cfg
            .kernel
            .as_ref()
            .ok_or_else(|| VmError::Backend("vm-kvm: kernel path is required".into()))?;
        let boot_plan = KvmBootPlan::from_config(cfg)?;
        let vm_fd = self
            .kvm
            .create_vm()
            .map_err(|e| VmError::Backend(format!("create VM: {e}")))?;
        vm_fd
            .set_tss_address(
                usize::try_from(KVM_TSS_ADDRESS)
                    .map_err(|_| VmError::Backend("vm-kvm: TSS address overflow".into()))?,
            )
            .map_err(|e| VmError::Backend(format!("set TSS address: {e}")))?;
        let pit_config = kvm_pit_config {
            flags: KVM_PIT_SPEAKER_DUMMY,
            ..Default::default()
        };
        vm_fd
            .create_irq_chip()
            .map_err(|e| VmError::Backend(format!("create irqchip: {e}")))?;
        vm_fd
            .create_pit2(pit_config)
            .map_err(|e| VmError::Backend(format!("create PIT: {e}")))?;
        register_guest_memory(&vm_fd, &boot_plan.guest_mem)?;

        let mut kernel_file = File::open(kernel)
            .map_err(|e| VmError::Backend(format!("open kernel {}: {e}", kernel.display())))?;
        let kernel_load = BzImage::load(
            &boot_plan.guest_mem,
            Some(boot_plan.kernel_load_addr),
            &mut kernel_file,
            Some(GuestAddress(HIMEM_START)),
        )
        .map_err(|e| VmError::Backend(format!("load bzImage {}: {e}", kernel.display())))?;
        let cmdline_size = boot_plan.cmdline_size()?;
        load_cmdline(
            &boot_plan.guest_mem,
            GuestAddress(CMDLINE_START),
            &boot_plan.cmdline,
        )
        .map_err(|e| VmError::Backend(format!("load kernel cmdline: {e}")))?;
        configure_linux_boot(
            &boot_plan.guest_mem,
            kernel_load.setup_header,
            GuestAddress(CMDLINE_START),
            cmdline_size,
        )?;

        Ok(KvmVmRuntime {
            vm_fd,
            boot_plan,
            entry_point: kernel_load.kernel_load,
            serial_output: Arc::new(Mutex::new(Vec::new())),
            real_mode: false,
        })
    }

    /// Build a runtime that executes `bytes` directly in 16-bit real
    /// mode at GPA 0. Skips the Linux bzImage / cmdline / GDT setup
    /// entirely — purely for tests / examples that exercise the KVM
    /// bring-up surface without a real kernel.
    fn build_flat_runtime(&self, cfg: &VmConfig, bytes: &[u8]) -> VmResult<KvmVmRuntime> {
        // `from_config_flat` skips the 2 MiB kernel-load-address
        // floor that the bzImage path requires — for a hand-rolled
        // real-mode test program, even a 4 KiB guest is plenty.
        let boot_plan = KvmBootPlan::from_config_flat(cfg)?;
        if (bytes.len() as u64) > boot_plan.memory_size_bytes() {
            return Err(VmError::Backend(format!(
                "vm-kvm: flat_binary ({} bytes) exceeds guest memory ({} bytes)",
                bytes.len(),
                boot_plan.memory_size_bytes(),
            )));
        }
        let vm_fd = self
            .kvm
            .create_vm()
            .map_err(|e| VmError::Backend(format!("create VM: {e}")))?;
        // Deliberately NOT calling create_irq_chip / create_pit2
        // here. With an in-kernel LAPIC active, `HLT` enters
        // halted-waiting-for-interrupt and KVM_RUN doesn't return
        // VcpuExit::Hlt to userspace — the vCPU thread would block
        // forever. Real-mode test code doesn't poke the PIC anyway,
        // so the minimal-bring-up that `hello_kvm` uses is correct
        // here too.
        register_guest_memory(&vm_fd, &boot_plan.guest_mem)?;
        // Write the program at GPA 0 via vm-memory's checked write —
        // no unsafe required at this layer.
        boot_plan
            .guest_mem
            .write_slice(bytes, GuestAddress(0))
            .map_err(|e| VmError::Backend(format!("write flat_binary at GPA 0: {e}")))?;
        Ok(KvmVmRuntime {
            vm_fd,
            boot_plan,
            entry_point: GuestAddress(0),
            serial_output: Arc::new(Mutex::new(Vec::new())),
            real_mode: true,
        })
    }

    fn spawn_vcpu(&self, id: VmId, runtime: &KvmVmRuntime) -> VmResult<KvmVcpuThread> {
        let mut vcpu = runtime
            .vm_fd
            .create_vcpu(0)
            .map_err(|e| VmError::Backend(format!("create vCPU for {id}: {e}")))?;
        if runtime.real_mode {
            configure_boot_vcpu_realmode(&self.kvm, &mut vcpu, runtime.entry_point)?;
        } else {
            configure_boot_vcpu(
                &self.kvm,
                &mut vcpu,
                &runtime.boot_plan.guest_mem,
                runtime.entry_point,
            )?;
        }
        let stop_requested = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop_requested);
        let serial_output = Arc::clone(&runtime.serial_output);
        let handle = thread::Builder::new()
            .name(format!("kvm-vcpu-{}", id.0))
            .spawn(move || run_vcpu_loop(vcpu, serial_output, stop_for_thread))
            .map_err(|e| VmError::Backend(format!("spawn vCPU thread for {id}: {e}")))?;
        Ok(KvmVcpuThread {
            stop_requested,
            handle,
        })
    }

    fn reap_finished_vcpus(inner: &mut Inner) -> VmResult<()> {
        for vm in inner.vms.values_mut() {
            if vm
                .vcpu
                .as_ref()
                .is_some_and(|vcpu| vcpu.handle.is_finished())
            {
                let vcpu = vm.vcpu.take().expect("checked is_some above");
                vm.state = VmState::Stopped;
                vm.last_run_error = Some(join_vcpu_thread(vcpu)?);
            }
        }
        Ok(())
    }

    fn stop_vm(vm: &mut KvmVm, kick_signal: c_int) -> VmResult<()> {
        let Some(vcpu) = vm.vcpu.take() else {
            vm.state = VmState::Stopped;
            return Ok(());
        };
        vcpu.stop_requested.store(true, Ordering::SeqCst);
        vcpu.handle
            .kill(kick_signal)
            .map_err(|e| VmError::Backend(format!("kick vCPU thread: {e}")))?;
        vm.last_run_error = Some(join_vcpu_thread(vcpu)?);
        vm.state = VmState::Stopped;
        Ok(())
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
    const DEFAULT_CMDLINE: &str = "console=ttyS0 reboot=k panic=1 pci=off";

    fn from_config(cfg: &VmConfig) -> VmResult<Self> {
        let mem_size = cfg
            .memory_mib
            .checked_mul(1024 * 1024)
            .ok_or_else(|| VmError::Backend("memory size overflow".into()))?;
        let mem_len = usize::try_from(mem_size)
            .map_err(|_| VmError::Backend(format!("memory size {mem_size} does not fit usize")))?;
        let guest_mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_len)])
            .map_err(|e| VmError::Backend(format!("guest memory layout: {e}")))?;
        if guest_mem.last_addr().raw_value() < Self::KERNEL_LOAD_ADDR {
            return Err(VmError::Backend(format!(
                "vm-kvm: guest memory must extend past kernel load address {:#x}",
                Self::KERNEL_LOAD_ADDR,
            )));
        }
        let mut cmdline = Cmdline::new(Self::CMDLINE_CAPACITY)
            .map_err(|e| VmError::Backend(format!("kernel cmdline capacity: {e}")))?;
        cmdline
            .insert_str(Self::DEFAULT_CMDLINE)
            .map_err(|e| VmError::Backend(format!("default kernel cmdline: {e}")))?;
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

    /// Like [`Self::from_config`] but skips the kernel-load-address
    /// floor and the kernel cmdline plumbing. Intended for the
    /// `flat_binary` boot mode — a hand-rolled real-mode program at
    /// GPA 0 doesn't need either, and the floor would reject the
    /// tiny guest sizes flat-binary tests use.
    fn from_config_flat(cfg: &VmConfig) -> VmResult<Self> {
        let mem_size = cfg
            .memory_mib
            .checked_mul(1024 * 1024)
            .ok_or_else(|| VmError::Backend("memory size overflow".into()))?;
        let mem_len = usize::try_from(mem_size)
            .map_err(|_| VmError::Backend(format!("memory size {mem_size} does not fit usize")))?;
        if mem_len == 0 {
            return Err(VmError::Backend(
                "vm-kvm: flat_binary mode requires memory_mib >= 1".into(),
            ));
        }
        let guest_mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_len)])
            .map_err(|e| VmError::Backend(format!("guest memory layout: {e}")))?;
        let cmdline = Cmdline::new(Self::CMDLINE_CAPACITY)
            .map_err(|e| VmError::Backend(format!("kernel cmdline capacity: {e}")))?;
        Ok(Self {
            guest_mem,
            kernel_load_addr: GuestAddress(0),
            initrd_load_addr: None,
            cmdline,
        })
    }

    fn cmdline_size(&self) -> VmResult<usize> {
        self.cmdline
            .as_cstring()
            .map(|cmdline| cmdline.as_bytes_with_nul().len())
            .map_err(|e| VmError::Backend(format!("kernel cmdline CString: {e}")))
    }

    fn cmdline_string(&self) -> VmResult<String> {
        self.cmdline
            .as_cstring()
            .map_err(|e| VmError::Backend(format!("kernel cmdline CString: {e}")))
            .and_then(|cmdline| {
                cmdline
                    .into_string()
                    .map_err(|e| VmError::Backend(format!("kernel cmdline UTF-8: {e}")))
            })
    }

    fn memory_size_bytes(&self) -> u64 {
        self.guest_mem.last_addr().raw_value() + 1
    }

    fn open_kvm() -> VmResult<Kvm> {
        Kvm::new().map_err(|e| VmError::Backend(format!("open /dev/kvm: {e}")))
    }
}

#[cfg(feature = "kvm")]
impl Hypervisor for KvmHypervisor {
    fn create_vm(&self, cfg: &VmConfig) -> VmResult<VmHandle> {
        let runtime = self.build_runtime(cfg)?;
        let id = VmId::next();
        let vm = KvmVm {
            config: cfg.clone(),
            state: VmState::Created,
            runtime,
            vcpu: None,
            last_run_error: None,
        };
        let mut inner = self.lock_inner()?;
        inner.vms.insert(id, vm);
        Ok(VmHandle {
            id,
            state: VmState::Created,
        })
    }

    fn start(&self, id: VmId) -> VmResult<()> {
        let mut inner = self.lock_inner()?;
        Self::reap_finished_vcpus(&mut inner)?;
        let vm = inner.vms.get_mut(&id).ok_or(VmError::UnknownVm(id))?;
        match vm.state {
            VmState::Created => {}
            VmState::Stopped => {
                vm.runtime = self.build_runtime(&vm.config)?;
                vm.last_run_error = None;
            }
            VmState::Running => {
                return Err(VmError::InvalidTransition {
                    id,
                    from: VmState::Running,
                    to: VmState::Running,
                })
            }
        }
        vm.vcpu = Some(self.spawn_vcpu(id, &vm.runtime)?);
        vm.state = VmState::Running;
        Ok(())
    }

    fn stop(&self, id: VmId) -> VmResult<()> {
        let mut inner = self.lock_inner()?;
        Self::reap_finished_vcpus(&mut inner)?;
        let vm = inner.vms.get_mut(&id).ok_or(VmError::UnknownVm(id))?;
        match vm.state {
            VmState::Running => Self::stop_vm(vm, self.kick_signal),
            other => Err(VmError::InvalidTransition {
                id,
                from: other,
                to: VmState::Stopped,
            }),
        }
    }

    fn state(&self, id: VmId) -> VmResult<VmState> {
        let mut inner = self.lock_inner()?;
        Self::reap_finished_vcpus(&mut inner)?;
        inner
            .vms
            .get(&id)
            .map(|vm| vm.state)
            .ok_or(VmError::UnknownVm(id))
    }

    fn snapshot(&self, _id: VmId) -> VmResult<SnapshotId> {
        Err(unsupported_snapshot())
    }

    fn restore(&self, _snap: SnapshotId) -> VmResult<VmHandle> {
        Err(unsupported_snapshot())
    }

    fn destroy(&self, id: VmId) -> VmResult<()> {
        let mut inner = self.lock_inner()?;
        Self::reap_finished_vcpus(&mut inner)?;
        let mut vm = inner.vms.remove(&id).ok_or(VmError::UnknownVm(id))?;
        if vm.state == VmState::Running {
            Self::stop_vm(&mut vm, self.kick_signal)?;
        }
        Ok(())
    }

    fn list_vms(&self) -> VmResult<Vec<VmHandle>> {
        let mut inner = self.lock_inner()?;
        Self::reap_finished_vcpus(&mut inner)?;
        Ok(inner
            .vms
            .iter()
            .map(|(id, vm)| VmHandle {
                id: *id,
                state: vm.state,
            })
            .collect())
    }

    fn list_snapshots(&self) -> VmResult<Vec<SnapshotId>> {
        Err(unsupported_snapshot())
    }

    fn delete_snapshot(&self, _snap: SnapshotId) -> VmResult<()> {
        Err(unsupported_snapshot())
    }

    fn snapshot_meta(&self, _snap: SnapshotId) -> VmResult<SnapshotMeta> {
        Err(unsupported_snapshot())
    }

    fn vm_meta(&self, id: VmId) -> VmResult<VmMeta> {
        let mut inner = self.lock_inner()?;
        Self::reap_finished_vcpus(&mut inner)?;
        let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
        Ok(VmMeta {
            id,
            state: vm.state,
            vcpus: vm.config.vcpus,
            memory_mib: vm.config.memory_mib,
            kernel_cmdline: vm.runtime.boot_plan.cmdline_string()?,
            snapshot_dir: vm.config.snapshot_dir.clone(),
        })
    }

    fn exec_in_guest(&self, _id: VmId, _req: GuestExecRequest) -> VmResult<GuestExecResult> {
        Err(VmError::Unsupported(
            "vm-kvm: guest exec requires the M2 vsock/agent path",
        ))
    }

    fn write_file(&self, _id: VmId, _path: String, _content: Vec<u8>, _mode: u32) -> VmResult<u64> {
        Err(VmError::Unsupported(
            "vm-kvm: guest file I/O requires the M2/M3 device path",
        ))
    }

    fn read_file(&self, _id: VmId, _path: String) -> VmResult<Vec<u8>> {
        Err(VmError::Unsupported(
            "vm-kvm: guest file I/O requires the M2/M3 device path",
        ))
    }
}

#[cfg(not(feature = "kvm"))]
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
fn register_guest_memory(vm_fd: &VmFd, guest_mem: &GuestMemoryMmap) -> VmResult<()> {
    for (slot, region) in guest_mem.iter().enumerate() {
        let userspace_addr = region
            .get_host_address(MemoryRegionAddress(0))
            .map_err(|e| {
                VmError::Backend(format!("resolve host address for memslot {slot}: {e}"))
            })? as u64;
        let mem_region = kvm_userspace_memory_region {
            slot: u32::try_from(slot)
                .map_err(|_| VmError::Backend(format!("too many guest memory regions: {slot}")))?,
            guest_phys_addr: region.start_addr().raw_value(),
            memory_size: region.len(),
            userspace_addr,
            flags: 0,
        };
        // SAFETY: `GuestMemoryMmap` owns the backing mapping for the lifetime of the VM runtime,
        // each memslot is registered once with a non-overlapping region, and the host address
        // returned by `vm-memory` points at the beginning of that live mapping.
        unsafe {
            vm_fd
                .set_user_memory_region(mem_region)
                .map_err(|e| VmError::Backend(format!("register memslot {slot}: {e}")))?;
        }
    }
    Ok(())
}

#[cfg(feature = "kvm")]
fn configure_linux_boot(
    guest_mem: &GuestMemoryMmap,
    setup_header: Option<setup_header>,
    cmdline_addr: GuestAddress,
    cmdline_size: usize,
) -> VmResult<()> {
    let mut params = boot_params::default();
    if let Some(header) = setup_header {
        params.hdr = header;
    }
    params.hdr.type_of_loader = KERNEL_LOADER_OTHER;
    params.hdr.boot_flag = KERNEL_BOOT_FLAG_MAGIC;
    params.hdr.header = KERNEL_HDR_MAGIC;
    params.hdr.kernel_alignment = KERNEL_MIN_ALIGNMENT_BYTES;
    params.hdr.cmd_line_ptr = u32::try_from(cmdline_addr.raw_value())
        .map_err(|_| VmError::Backend("vm-kvm: cmdline address does not fit boot params".into()))?;
    params.hdr.cmdline_size = u32::try_from(cmdline_size)
        .map_err(|_| VmError::Backend("vm-kvm: cmdline size does not fit boot params".into()))?;
    add_e820_entry(&mut params, 0, SYSTEM_MEM_START, E820_RAM)?;
    add_e820_entry(
        &mut params,
        SYSTEM_MEM_START,
        SYSTEM_MEM_SIZE,
        E820_RESERVED,
    )?;
    let mem_end = guest_mem.last_addr().raw_value() + 1;
    if mem_end > HIMEM_START {
        add_e820_entry(&mut params, HIMEM_START, mem_end - HIMEM_START, E820_RAM)?;
    }
    LinuxBootConfigurator::write_bootparams(
        &BootParams::new(&params, GuestAddress(ZERO_PAGE_START)),
        guest_mem,
    )
    .map_err(|e| VmError::Backend(format!("write Linux boot params: {e}")))
}

#[cfg(feature = "kvm")]
fn add_e820_entry(params: &mut boot_params, addr: u64, size: u64, mem_type: u32) -> VmResult<()> {
    if usize::from(params.e820_entries) >= params.e820_table.len() {
        return Err(VmError::Backend("vm-kvm: e820 table is full".into()));
    }
    let entry = &mut params.e820_table[usize::from(params.e820_entries)];
    entry.addr = addr;
    entry.size = size;
    entry.type_ = mem_type;
    params.e820_entries += 1;
    Ok(())
}

#[cfg(feature = "kvm")]
fn configure_boot_vcpu(
    kvm: &Kvm,
    vcpu: &mut VcpuFd,
    guest_mem: &GuestMemoryMmap,
    entry_point: GuestAddress,
) -> VmResult<()> {
    let cpuid = kvm
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(|e| VmError::Backend(format!("get supported CPUID: {e}")))?;
    vcpu.set_cpuid2(&cpuid)
        .map_err(|e| VmError::Backend(format!("set CPUID: {e}")))?;
    setup_fpu(vcpu)?;
    setup_regs(vcpu, entry_point)?;
    setup_sregs(guest_mem, vcpu)?;
    Ok(())
}

/// Bring up a vCPU in 16-bit real mode at `CS:base=0, selector=0,
/// rip=entry_point`. Mirrors what the `hello_kvm` example does for
/// the raw kvm-ioctls path, just shared through the Hypervisor
/// trait. Skips the protected-mode / long-mode / paging setup
/// `setup_sregs` does for kernel boots — propagating that to a
/// hand-rolled real-mode binary would leave the vCPU unable to
/// execute the program at GPA 0.
#[cfg(feature = "kvm")]
fn configure_boot_vcpu_realmode(
    kvm: &Kvm,
    vcpu: &mut VcpuFd,
    entry_point: GuestAddress,
) -> VmResult<()> {
    let cpuid = kvm
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(|e| VmError::Backend(format!("get supported CPUID: {e}")))?;
    vcpu.set_cpuid2(&cpuid)
        .map_err(|e| VmError::Backend(format!("set CPUID: {e}")))?;
    setup_fpu(vcpu)?;
    let mut sregs = vcpu
        .get_sregs()
        .map_err(|e| VmError::Backend(format!("get sregs (realmode): {e}")))?;
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    vcpu.set_sregs(&sregs)
        .map_err(|e| VmError::Backend(format!("set sregs (realmode): {e}")))?;
    let regs = kvm_regs {
        rip: entry_point.raw_value(),
        rflags: 0x2,
        ..Default::default()
    };
    vcpu.set_regs(&regs)
        .map_err(|e| VmError::Backend(format!("set regs (realmode): {e}")))?;
    Ok(())
}

#[cfg(feature = "kvm")]
fn setup_fpu(vcpu: &VcpuFd) -> VmResult<()> {
    let fpu = kvm_fpu {
        fcw: 0x37f,
        mxcsr: 0x1f80,
        ..Default::default()
    };
    vcpu.set_fpu(&fpu)
        .map_err(|e| VmError::Backend(format!("set FPU registers: {e}")))
}

#[cfg(feature = "kvm")]
fn setup_regs(vcpu: &VcpuFd, entry_point: GuestAddress) -> VmResult<()> {
    let regs = kvm_regs {
        rflags: 0x2,
        rip: entry_point.raw_value(),
        rsp: BOOT_STACK_POINTER,
        rbp: BOOT_STACK_POINTER,
        rsi: ZERO_PAGE_START,
        ..Default::default()
    };
    vcpu.set_regs(&regs)
        .map_err(|e| VmError::Backend(format!("set base registers: {e}")))
}

#[cfg(feature = "kvm")]
fn setup_sregs(guest_mem: &GuestMemoryMmap, vcpu: &VcpuFd) -> VmResult<()> {
    let mut sregs = vcpu
        .get_sregs()
        .map_err(|e| VmError::Backend(format!("get special registers: {e}")))?;
    configure_segments_and_sregs(guest_mem, &mut sregs)?;
    setup_page_tables(guest_mem, &mut sregs)?;
    vcpu.set_sregs(&sregs)
        .map_err(|e| VmError::Backend(format!("set special registers: {e}")))
}

#[cfg(feature = "kvm")]
fn configure_segments_and_sregs(
    guest_mem: &GuestMemoryMmap,
    sregs: &mut kvm_sregs,
) -> VmResult<()> {
    let gdt_table = [
        gdt_entry(0, 0, 0),
        gdt_entry(0xa09b, 0, 0xfffff),
        gdt_entry(0xc093, 0, 0xfffff),
        gdt_entry(0x808b, 0, 0xfffff),
    ];
    let code_seg = kvm_segment_from_gdt(gdt_table[1], 1);
    let data_seg = kvm_segment_from_gdt(gdt_table[2], 2);
    let tss_seg = kvm_segment_from_gdt(gdt_table[3], 3);

    write_gdt_table(&gdt_table, guest_mem)?;
    sregs.gdt.base = BOOT_GDT_OFFSET;
    sregs.gdt.limit = u16::try_from(mem::size_of_val(&gdt_table) - 1)
        .map_err(|_| VmError::Backend("vm-kvm: GDT limit overflow".into()))?;

    write_idt_value(0, guest_mem)?;
    sregs.idt.base = BOOT_IDT_OFFSET;
    sregs.idt.limit = u16::try_from(mem::size_of::<u64>() - 1)
        .map_err(|_| VmError::Backend("vm-kvm: IDT limit overflow".into()))?;

    sregs.cs = code_seg;
    sregs.ds = data_seg;
    sregs.es = data_seg;
    sregs.fs = data_seg;
    sregs.gs = data_seg;
    sregs.ss = data_seg;
    sregs.tr = tss_seg;
    sregs.cr0 |= X86_CR0_PE;
    sregs.efer |= EFER_LME | EFER_LMA;
    Ok(())
}

#[cfg(feature = "kvm")]
fn write_gdt_table(table: &[u64], guest_mem: &GuestMemoryMmap) -> VmResult<()> {
    let boot_gdt_addr = GuestAddress(BOOT_GDT_OFFSET);
    for (index, entry) in table.iter().enumerate() {
        let addr = guest_mem
            .checked_offset(boot_gdt_addr, index * mem::size_of::<u64>())
            .ok_or_else(|| VmError::Backend("vm-kvm: GDT write overflow".into()))?;
        guest_mem
            .write_obj(*entry, addr)
            .map_err(|e| VmError::Backend(format!("write GDT entry {index}: {e}")))?;
    }
    Ok(())
}

#[cfg(feature = "kvm")]
fn write_idt_value(value: u64, guest_mem: &GuestMemoryMmap) -> VmResult<()> {
    guest_mem
        .write_obj(value, GuestAddress(BOOT_IDT_OFFSET))
        .map_err(|e| VmError::Backend(format!("write IDT: {e}")))
}

#[cfg(feature = "kvm")]
fn setup_page_tables(guest_mem: &GuestMemoryMmap, sregs: &mut kvm_sregs) -> VmResult<()> {
    let boot_pml4_addr = GuestAddress(PML4_START);
    let boot_pdpte_addr = GuestAddress(PDPTE_START);
    let boot_pde_addr = GuestAddress(PDE_START);

    guest_mem
        .write_obj(boot_pdpte_addr.raw_value() | 0x03, boot_pml4_addr)
        .map_err(|e| VmError::Backend(format!("write PML4 entry: {e}")))?;
    guest_mem
        .write_obj(boot_pde_addr.raw_value() | 0x03, boot_pdpte_addr)
        .map_err(|e| VmError::Backend(format!("write PDPTE entry: {e}")))?;
    for index in 0..512u64 {
        guest_mem
            .write_obj((index << 21) | 0x83, GuestAddress(PDE_START + (index * 8)))
            .map_err(|e| VmError::Backend(format!("write PDE entry {index}: {e}")))?;
    }

    sregs.cr3 = boot_pml4_addr.raw_value();
    sregs.cr4 |= X86_CR4_PAE;
    sregs.cr0 |= X86_CR0_PG | X86_CR0_ET;
    Ok(())
}

#[cfg(feature = "kvm")]
fn run_vcpu_loop(
    mut vcpu: VcpuFd,
    serial_output: Arc<Mutex<Vec<u8>>>,
    stop_requested: Arc<AtomicBool>,
) -> VmResult<()> {
    loop {
        match vcpu.run() {
            Ok(VcpuExit::IoOut(port, data)) => handle_io_out(port, data, &serial_output)?,
            Ok(VcpuExit::IoIn(port, data)) => handle_io_in(port, data),
            Ok(VcpuExit::MmioRead(_, data)) => data.fill(0),
            Ok(VcpuExit::MmioWrite(_, _)) => {}
            Ok(VcpuExit::Hlt) => break,
            Ok(VcpuExit::Shutdown) => {
                // Shutdown == triple fault (or an explicit guest
                // shutdown). During kernel bring-up it almost always
                // means the guest faulted on or near entry. Capture
                // register state so the failure is diagnosable rather
                // than a silent "0 bytes, Stopped". A real-mode test
                // program that HLTs hits the Hlt arm above, so this
                // path is kernel-boot-specific.
                let diag = match vcpu.get_regs() {
                    Ok(r) => format!(
                        "vcpu SHUTDOWN (triple fault?): rip={:#x} rsp={:#x} rflags={:#x} \
                         rax={:#x} rbx={:#x} rsi={:#x} rdi={:#x}",
                        r.rip, r.rsp, r.rflags, r.rax, r.rbx, r.rsi, r.rdi,
                    ),
                    Err(e) => format!("vcpu SHUTDOWN (triple fault?); get_regs failed: {e}"),
                };
                return Err(VmError::Backend(diag));
            }
            Ok(VcpuExit::Intr) if stop_requested.load(Ordering::SeqCst) => break,
            Ok(VcpuExit::FailEntry(reason, cpu)) => {
                return Err(VmError::Backend(format!(
                    "KVM fail entry: reason={reason:#x} cpu={cpu}",
                )))
            }
            Ok(VcpuExit::InternalError) => {
                return Err(VmError::Backend("KVM internal error exit".into()))
            }
            Ok(other) => {
                return Err(VmError::Backend(format!(
                    "unexpected KVM vCPU exit: {other:?}",
                )))
            }
            Err(err) if err.errno() == EINTR && stop_requested.load(Ordering::SeqCst) => break,
            Err(err) if err.errno() == EINTR => continue,
            Err(err) => return Err(VmError::Backend(format!("KVM_RUN failed: {err}"))),
        }
    }
    Ok(())
}

#[cfg(feature = "kvm")]
fn handle_io_out(port: u16, data: &[u8], serial_output: &Arc<Mutex<Vec<u8>>>) -> VmResult<()> {
    if port == SERIAL_PORT_BASE {
        serial_output
            .lock()
            .map_err(|_| VmError::Backend("vm-kvm: serial output mutex poisoned".into()))?
            .extend_from_slice(data);
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(data)
            .and_then(|_| stdout.flush())
            .map_err(|e| VmError::Backend(format!("write serial output: {e}")))?;
    }
    Ok(())
}

#[cfg(feature = "kvm")]
fn handle_io_in(port: u16, data: &mut [u8]) {
    data.fill(0);
    if data.is_empty() {
        return;
    }
    match port {
        port if port == SERIAL_PORT_BASE + 5 => data[0] = 0x60,
        port if port == SERIAL_PORT_BASE + 2 => data[0] = 0x01,
        _ => {}
    }
}

#[cfg(feature = "kvm")]
fn join_vcpu_thread(vcpu: KvmVcpuThread) -> VmResult<String> {
    match vcpu.handle.join() {
        Ok(Ok(())) => Ok(String::new()),
        Ok(Err(err)) => Ok(err.to_string()),
        Err(_) => Err(VmError::Backend("vm-kvm: vCPU thread panicked".into())),
    }
}

#[cfg(feature = "kvm")]
static KICK_SIGNAL_INIT: OnceLock<Result<c_int, String>> = OnceLock::new();

#[cfg(feature = "kvm")]
fn vcpu_kick_signal() -> VmResult<c_int> {
    KICK_SIGNAL_INIT
        .get_or_init(|| {
            let signal = libc::SIGRTMIN();
            register_signal_handler(signal, handle_vcpu_kick)
                .map_err(|e| format!("register vCPU kick signal handler: {e}"))?;
            Ok(signal)
        })
        .clone()
        .map_err(VmError::Backend)
}

#[cfg(feature = "kvm")]
extern "C" fn handle_vcpu_kick(_: c_int, _: *mut siginfo_t, _: *mut c_void) {}

#[cfg(feature = "kvm")]
fn unsupported_snapshot() -> VmError {
    VmError::Unsupported("vm-kvm: snapshot lifecycle is not implemented in M1")
}

#[cfg(not(feature = "kvm"))]
fn unsupported() -> VmError {
    VmError::Unsupported("vm-kvm: build without the `kvm` feature")
}

#[cfg(feature = "kvm")]
const SERIAL_PORT_BASE: u16 = 0x3f8;
#[cfg(feature = "kvm")]
const BOOT_STACK_POINTER: u64 = 0x8ff0;
#[cfg(feature = "kvm")]
const CMDLINE_START: u64 = 0x20_000;
#[cfg(feature = "kvm")]
const ZERO_PAGE_START: u64 = 0x7000;
#[cfg(feature = "kvm")]
const HIMEM_START: u64 = 0x10_0000;
#[cfg(feature = "kvm")]
const SYSTEM_MEM_START: u64 = 0x9fc00;
#[cfg(feature = "kvm")]
const SYSTEM_MEM_SIZE: u64 = HIMEM_START - SYSTEM_MEM_START;
#[cfg(feature = "kvm")]
const KVM_TSS_ADDRESS: u64 = 0xfffb_d000;
#[cfg(feature = "kvm")]
const KERNEL_BOOT_FLAG_MAGIC: u16 = 0xaa55;
#[cfg(feature = "kvm")]
const KERNEL_HDR_MAGIC: u32 = 0x5372_6448;
#[cfg(feature = "kvm")]
const KERNEL_LOADER_OTHER: u8 = 0xff;
#[cfg(feature = "kvm")]
const KERNEL_MIN_ALIGNMENT_BYTES: u32 = 0x0100_0000;
#[cfg(feature = "kvm")]
const E820_RAM: u32 = 1;
#[cfg(feature = "kvm")]
const E820_RESERVED: u32 = 2;
#[cfg(feature = "kvm")]
const BOOT_GDT_OFFSET: u64 = 0x500;
#[cfg(feature = "kvm")]
const BOOT_IDT_OFFSET: u64 = 0x520;
#[cfg(feature = "kvm")]
const PML4_START: u64 = 0x9000;
#[cfg(feature = "kvm")]
const PDPTE_START: u64 = 0xa000;
#[cfg(feature = "kvm")]
const PDE_START: u64 = 0xb000;
#[cfg(feature = "kvm")]
const EFER_LMA: u64 = 0x400;
#[cfg(feature = "kvm")]
const EFER_LME: u64 = 0x100;
#[cfg(feature = "kvm")]
const X86_CR0_PE: u64 = 0x1;
#[cfg(feature = "kvm")]
const X86_CR0_ET: u64 = 0x10;
#[cfg(feature = "kvm")]
const X86_CR0_PG: u64 = 0x8000_0000;
#[cfg(feature = "kvm")]
const X86_CR4_PAE: u64 = 0x20;

#[cfg(feature = "kvm")]
fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    ((u64::from(base) & 0xff00_0000) << 32)
        | ((u64::from(flags) & 0x0000_f0ff) << 40)
        | ((u64::from(limit) & 0x000f_0000) << 32)
        | ((u64::from(base) & 0x00ff_ffff) << 16)
        | (u64::from(limit) & 0x0000_ffff)
}

#[cfg(feature = "kvm")]
fn get_base(entry: u64) -> u64 {
    ((entry & 0xff00_0000_0000_0000) >> 32)
        | ((entry & 0x0000_00ff_0000_0000) >> 16)
        | ((entry & 0x0000_0000_ffff_0000) >> 16)
}

#[cfg(feature = "kvm")]
fn get_limit(entry: u64) -> u32 {
    let limit =
        ((((entry) & 0x000f_0000_0000_0000) >> 32) | ((entry) & 0x0000_0000_0000_ffff)) as u32;
    if get_g(entry) == 0 {
        limit
    } else {
        (limit << 12) | 0x0fff
    }
}

#[cfg(feature = "kvm")]
fn get_g(entry: u64) -> u8 {
    ((entry & 0x0080_0000_0000_0000) >> 55) as u8
}

#[cfg(feature = "kvm")]
fn get_db(entry: u64) -> u8 {
    ((entry & 0x0040_0000_0000_0000) >> 54) as u8
}

#[cfg(feature = "kvm")]
fn get_l(entry: u64) -> u8 {
    ((entry & 0x0020_0000_0000_0000) >> 53) as u8
}

#[cfg(feature = "kvm")]
fn get_avl(entry: u64) -> u8 {
    ((entry & 0x0010_0000_0000_0000) >> 52) as u8
}

#[cfg(feature = "kvm")]
fn get_p(entry: u64) -> u8 {
    ((entry & 0x0000_8000_0000_0000) >> 47) as u8
}

#[cfg(feature = "kvm")]
fn get_dpl(entry: u64) -> u8 {
    ((entry & 0x0000_6000_0000_0000) >> 45) as u8
}

#[cfg(feature = "kvm")]
fn get_s(entry: u64) -> u8 {
    ((entry & 0x0000_1000_0000_0000) >> 44) as u8
}

#[cfg(feature = "kvm")]
fn get_type(entry: u64) -> u8 {
    ((entry & 0x0000_0f00_0000_0000) >> 40) as u8
}

#[cfg(feature = "kvm")]
fn kvm_segment_from_gdt(entry: u64, table_index: u8) -> kvm_segment {
    kvm_segment {
        base: get_base(entry),
        limit: get_limit(entry),
        selector: u16::from(table_index) * 8,
        type_: get_type(entry),
        present: get_p(entry),
        dpl: get_dpl(entry),
        db: get_db(entry),
        s: get_s(entry),
        l: get_l(entry),
        g: get_g(entry),
        avl: get_avl(entry),
        padding: 0,
        unusable: if get_p(entry) == 0 { 1 } else { 0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(feature = "kvm"))]
    fn all_methods_return_unsupported_without_kvm_feature() {
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
    fn boot_plan_derives_memory_and_cmdline() {
        let plan = KvmHypervisor::boot_plan(&VmConfig {
            memory_mib: 64,
            cmdline: "foo=bar".into(),
            ..VmConfig::default()
        })
        .expect("boot plan");
        let cmdline = plan.cmdline.as_cstring().expect("cmdline CString");
        let cmdline = cmdline.to_str().expect("utf-8 cmdline");
        assert_eq!(plan.memory_size_bytes(), 64 * 1024 * 1024);
        assert_eq!(plan.kernel_load_addr, GuestAddress(0x20_0000));
        assert!(plan.initrd_load_addr.is_none());
        assert!(cmdline.contains("console=ttyS0"));
        assert!(cmdline.contains("reboot=k"));
        assert!(cmdline.contains("foo=bar"));
    }

    #[cfg(feature = "kvm")]
    #[test]
    fn configure_linux_boot_writes_zero_page_and_e820() {
        let plan = KvmHypervisor::boot_plan(&VmConfig {
            memory_mib: 64,
            ..VmConfig::default()
        })
        .expect("boot plan");
        let cmdline_size = plan.cmdline_size().expect("cmdline size");
        configure_linux_boot(
            &plan.guest_mem,
            Some(setup_header::default()),
            GuestAddress(CMDLINE_START),
            cmdline_size,
        )
        .expect("boot params");
        let params: boot_params = plan
            .guest_mem
            .read_obj(GuestAddress(ZERO_PAGE_START))
            .expect("read zero page");
        let boot_flag = params.hdr.boot_flag;
        let header = params.hdr.header;
        let cmd_line_ptr = params.hdr.cmd_line_ptr;
        let header_cmdline_size = params.hdr.cmdline_size;
        let e820_entries = params.e820_entries;
        let entry0_addr = params.e820_table[0].addr;
        let entry0_type = params.e820_table[0].type_;
        let entry1_addr = params.e820_table[1].addr;
        let entry1_type = params.e820_table[1].type_;
        let entry2_addr = params.e820_table[2].addr;
        let entry2_type = params.e820_table[2].type_;
        assert_eq!(boot_flag, KERNEL_BOOT_FLAG_MAGIC);
        assert_eq!(header, KERNEL_HDR_MAGIC);
        assert_eq!(cmd_line_ptr, CMDLINE_START as u32);
        assert_eq!(header_cmdline_size, cmdline_size as u32);
        assert_eq!(e820_entries, 3);
        assert_eq!(entry0_addr, 0);
        assert_eq!(entry0_type, E820_RAM);
        assert_eq!(entry1_addr, SYSTEM_MEM_START);
        assert_eq!(entry1_type, E820_RESERVED);
        assert_eq!(entry2_addr, HIMEM_START);
        assert_eq!(entry2_type, E820_RAM);
    }

    #[cfg(feature = "kvm")]
    #[test]
    fn create_vm_requires_kernel_path() {
        let hv = match KvmHypervisor::new() {
            Ok(hv) => hv,
            Err(err) => {
                assert!(err.to_string().contains("/dev/kvm"));
                return;
            }
        };
        let err = hv.create_vm(&VmConfig::default()).unwrap_err();
        assert!(err.to_string().contains("kernel path is required"));
    }
}
