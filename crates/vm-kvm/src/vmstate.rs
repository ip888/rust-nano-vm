//! Capture and restore of KVM vCPU + machine state for snapshots.
//!
//! This is the KVM-specific half of "snapshot once, fork many": the
//! `snapshot` crate owns the portable on-disk layout (manifest + memory
//! backing file), and this module owns the architectural state only KVM
//! knows about — the vCPU register file (regs, sregs, FPU/XSAVE, LAPIC,
//! MSRs, ...) and (in a later slice) the in-kernel machine devices.
//!
//! Each fixed-size KVM struct is stored as its raw `#[repr(C)]` bytes.
//! That makes a snapshot a **same-host / same-kernel** artifact, which is
//! exactly the fork use case (restore on the machine that captured it).
//! Cross-kernel migration would need a versioned, field-wise schema; we
//! deliberately don't promise that yet. The byte length of each blob is
//! checked on restore so a struct-layout change between capture and
//! restore is caught loudly instead of corrupting vCPU state.

#![cfg(feature = "kvm")]
// This slice lands the state codec; `snapshot()` / `restore()` wire it in
// the next slice (the vCPU pause mechanism + VM reconstruction). Until then
// the capture/restore entry points have no in-crate caller.
#![allow(dead_code)]

use std::path::Path;

use kvm_bindings::{
    kvm_clock_data, kvm_debugregs, kvm_fpu, kvm_irqchip, kvm_lapic_state, kvm_mp_state,
    kvm_pit_state2, kvm_regs, kvm_sregs, kvm_vcpu_events, kvm_xcrs, kvm_xsave, Msrs,
    KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER, KVM_IRQCHIP_PIC_SLAVE,
};
use kvm_ioctls::{Kvm, VcpuFd, VmFd};
use serde::{Deserialize, Serialize};
use vm_core::{VmError, VmResult};

/// Filename of the vCPU-state sidecar inside a snapshot directory.
pub const VCPU_STATE_FILE: &str = "vcpu0.json";
/// Filename of the machine-state sidecar inside a snapshot directory.
pub const MACHINE_STATE_FILE: &str = "machine.json";

/// Full architectural state of a single vCPU, enough to resume it exactly
/// where a snapshot froze it. Each `*_blob` is the raw bytes of the
/// corresponding KVM struct (see module docs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VcpuState {
    /// `kvm_regs` — general-purpose registers + rip/rflags.
    pub regs: Vec<u8>,
    /// `kvm_sregs` — segment/control registers (CR0..4, EFER, GDT/IDT).
    pub sregs: Vec<u8>,
    /// `kvm_fpu` — legacy x87/SSE FPU state.
    pub fpu: Vec<u8>,
    /// `kvm_xcrs` — extended control registers (XCR0).
    pub xcrs: Vec<u8>,
    /// `kvm_debugregs` — hardware debug registers (DR0..7).
    pub debug_regs: Vec<u8>,
    /// `kvm_mp_state` — run/halt state.
    pub mp_state: Vec<u8>,
    /// `kvm_vcpu_events` — pending exception/interrupt/NMI/SMI state.
    pub vcpu_events: Vec<u8>,
    /// `kvm_lapic_state` — local APIC register page (1 KiB).
    pub lapic: Vec<u8>,
    /// `kvm_xsave` — XSAVE area (extended FPU/AVX state).
    pub xsave: Vec<u8>,
    /// Model-specific registers, as `(index, value)` pairs.
    pub msrs: Vec<MsrEntry>,
}

/// One captured MSR: its index and value.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MsrEntry {
    /// MSR index (e.g. `MSR_IA32_TSC`).
    pub index: u32,
    /// MSR value at capture time.
    pub value: u64,
}

impl VcpuState {
    /// Capture the full state of `vcpu`. `msr_indices` is the set of MSRs to
    /// snapshot (see [`snapshotable_msr_indices`]).
    pub fn capture(vcpu: &VcpuFd, msr_indices: &[u32]) -> VmResult<Self> {
        Ok(Self {
            regs: pod_to_bytes(&vcpu.get_regs().map_err(kvm_err("get_regs"))?),
            sregs: pod_to_bytes(&vcpu.get_sregs().map_err(kvm_err("get_sregs"))?),
            fpu: pod_to_bytes(&vcpu.get_fpu().map_err(kvm_err("get_fpu"))?),
            xcrs: pod_to_bytes(&vcpu.get_xcrs().map_err(kvm_err("get_xcrs"))?),
            debug_regs: pod_to_bytes(&vcpu.get_debug_regs().map_err(kvm_err("get_debug_regs"))?),
            mp_state: pod_to_bytes(&vcpu.get_mp_state().map_err(kvm_err("get_mp_state"))?),
            vcpu_events: pod_to_bytes(&vcpu.get_vcpu_events().map_err(kvm_err("get_vcpu_events"))?),
            lapic: pod_to_bytes(&vcpu.get_lapic().map_err(kvm_err("get_lapic"))?),
            // kvm_xsave is a flexible-array struct (not Copy); the fixed
            // 1024-word `region` covers the standard XSAVE area our guests use.
            xsave: pod_to_bytes(&vcpu.get_xsave().map_err(kvm_err("get_xsave"))?.region),
            msrs: capture_msrs(vcpu, msr_indices)?,
        })
    }

    /// Restore this state onto a freshly created `vcpu`. Ordering follows
    /// common VMM practice: sregs before regs, MSRs before lapic, events
    /// and mp_state last, so dependent state lands consistently.
    pub fn restore(&self, vcpu: &VcpuFd) -> VmResult<()> {
        vcpu.set_sregs(&pod_from_bytes::<kvm_sregs>(&self.sregs, "sregs")?)
            .map_err(kvm_err("set_sregs"))?;
        vcpu.set_regs(&pod_from_bytes::<kvm_regs>(&self.regs, "regs")?)
            .map_err(kvm_err("set_regs"))?;
        vcpu.set_fpu(&pod_from_bytes::<kvm_fpu>(&self.fpu, "fpu")?)
            .map_err(kvm_err("set_fpu"))?;
        vcpu.set_xcrs(&pod_from_bytes::<kvm_xcrs>(&self.xcrs, "xcrs")?)
            .map_err(kvm_err("set_xcrs"))?;
        vcpu.set_debug_regs(&pod_from_bytes::<kvm_debugregs>(
            &self.debug_regs,
            "debug_regs",
        )?)
        .map_err(kvm_err("set_debug_regs"))?;
        restore_msrs(vcpu, &self.msrs)?;
        restore_xsave(vcpu, &self.xsave)?;
        vcpu.set_lapic(&pod_from_bytes::<kvm_lapic_state>(&self.lapic, "lapic")?)
            .map_err(kvm_err("set_lapic"))?;
        vcpu.set_vcpu_events(&pod_from_bytes::<kvm_vcpu_events>(
            &self.vcpu_events,
            "vcpu_events",
        )?)
        .map_err(kvm_err("set_vcpu_events"))?;
        // set_mp_state takes the (Copy) struct by value, unlike the others.
        vcpu.set_mp_state(pod_from_bytes::<kvm_mp_state>(&self.mp_state, "mp_state")?)
            .map_err(kvm_err("set_mp_state"))?;
        Ok(())
    }

    /// Serialize to `<dir>/vcpu0.json`.
    pub fn write_to_dir(&self, dir: &Path) -> VmResult<()> {
        let bytes = serde_json::to_vec(self)
            .map_err(|e| VmError::Backend(format!("vm-kvm: serialize vcpu state: {e}")))?;
        std::fs::write(dir.join(VCPU_STATE_FILE), bytes)
            .map_err(|e| VmError::Backend(format!("vm-kvm: write vcpu state: {e}")))
    }

    /// Read back from `<dir>/vcpu0.json`.
    pub fn read_from_dir(dir: &Path) -> VmResult<Self> {
        let bytes = std::fs::read(dir.join(VCPU_STATE_FILE))
            .map_err(|e| VmError::Backend(format!("vm-kvm: read vcpu state: {e}")))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| VmError::Backend(format!("vm-kvm: parse vcpu state: {e}")))
    }
}

/// In-kernel machine device state: the PIC pair, IOAPIC, PIT, and the KVM
/// paravirtual clock. Captured alongside the vCPU so timers and interrupt
/// routing resume coherently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineState {
    /// `kvm_irqchip` for the master 8259 PIC.
    pub pic_master: Vec<u8>,
    /// `kvm_irqchip` for the slave 8259 PIC.
    pub pic_slave: Vec<u8>,
    /// `kvm_irqchip` for the IOAPIC.
    pub ioapic: Vec<u8>,
    /// `kvm_pit_state2` — the 8254 PIT.
    pub pit: Vec<u8>,
    /// `kvm_clock_data` — the KVM paravirtual clock.
    pub clock: Vec<u8>,
}

impl MachineState {
    /// Capture the in-kernel machine device state from `vm`.
    pub fn capture(vm: &VmFd) -> VmResult<Self> {
        Ok(Self {
            pic_master: capture_irqchip(vm, KVM_IRQCHIP_PIC_MASTER)?,
            pic_slave: capture_irqchip(vm, KVM_IRQCHIP_PIC_SLAVE)?,
            ioapic: capture_irqchip(vm, KVM_IRQCHIP_IOAPIC)?,
            pit: pod_to_bytes(&vm.get_pit2().map_err(kvm_err("get_pit2"))?),
            clock: pod_to_bytes(&vm.get_clock()),
        })
    }

    /// Restore the machine device state onto `vm`.
    pub fn restore(&self, vm: &VmFd) -> VmResult<()> {
        restore_irqchip(vm, &self.pic_master, "pic_master")?;
        restore_irqchip(vm, &self.pic_slave, "pic_slave")?;
        restore_irqchip(vm, &self.ioapic, "ioapic")?;
        vm.set_pit2(&pod_from_bytes::<kvm_pit_state2>(&self.pit, "pit")?)
            .map_err(kvm_err("set_pit2"))?;
        let clock = pod_from_bytes::<kvm_clock_data>(&self.clock, "clock")?;
        vm.set_clock(&clock).map_err(kvm_err("set_clock"))?;
        Ok(())
    }

    /// Serialize to `<dir>/machine.json`.
    pub fn write_to_dir(&self, dir: &Path) -> VmResult<()> {
        let bytes = serde_json::to_vec(self)
            .map_err(|e| VmError::Backend(format!("vm-kvm: serialize machine state: {e}")))?;
        std::fs::write(dir.join(MACHINE_STATE_FILE), bytes)
            .map_err(|e| VmError::Backend(format!("vm-kvm: write machine state: {e}")))
    }

    /// Read back from `<dir>/machine.json`.
    pub fn read_from_dir(dir: &Path) -> VmResult<Self> {
        let bytes = std::fs::read(dir.join(MACHINE_STATE_FILE))
            .map_err(|e| VmError::Backend(format!("vm-kvm: read machine state: {e}")))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| VmError::Backend(format!("vm-kvm: parse machine state: {e}")))
    }
}

/// Read one `kvm_irqchip` (selected by `chip_id`) as raw bytes.
fn capture_irqchip(vm: &VmFd, chip_id: u32) -> VmResult<Vec<u8>> {
    let mut chip = kvm_irqchip {
        chip_id,
        ..Default::default()
    };
    vm.get_irqchip(&mut chip).map_err(kvm_err("get_irqchip"))?;
    Ok(pod_to_bytes(&chip))
}

/// Restore one `kvm_irqchip` from raw bytes.
fn restore_irqchip(vm: &VmFd, bytes: &[u8], what: &str) -> VmResult<()> {
    let chip = pod_from_bytes::<kvm_irqchip>(bytes, what)?;
    vm.set_irqchip(&chip).map_err(kvm_err("set_irqchip"))?;
    Ok(())
}

/// Read every MSR in `indices` from the vCPU.
fn capture_msrs(vcpu: &VcpuFd, indices: &[u32]) -> VmResult<Vec<MsrEntry>> {
    if indices.is_empty() {
        return Ok(Vec::new());
    }
    let entries: Vec<kvm_bindings::kvm_msr_entry> = indices
        .iter()
        .map(|&index| kvm_bindings::kvm_msr_entry {
            index,
            ..Default::default()
        })
        .collect();
    let mut msrs = Msrs::from_entries(&entries)
        .map_err(|e| VmError::Backend(format!("vm-kvm: build MSR list: {e:?}")))?;
    let read = vcpu.get_msrs(&mut msrs).map_err(kvm_err("get_msrs"))?;
    Ok(msrs.as_slice()[..read]
        .iter()
        .map(|e| MsrEntry {
            index: e.index,
            value: e.data,
        })
        .collect())
}

/// Write captured MSRs back to the vCPU.
fn restore_msrs(vcpu: &VcpuFd, msrs: &[MsrEntry]) -> VmResult<()> {
    if msrs.is_empty() {
        return Ok(());
    }
    let entries: Vec<kvm_bindings::kvm_msr_entry> = msrs
        .iter()
        .map(|m| kvm_bindings::kvm_msr_entry {
            index: m.index,
            data: m.value,
            ..Default::default()
        })
        .collect();
    let msrs = Msrs::from_entries(&entries)
        .map_err(|e| VmError::Backend(format!("vm-kvm: build MSR list: {e:?}")))?;
    let written = vcpu.set_msrs(&msrs).map_err(kvm_err("set_msrs"))?;
    if written != msrs.as_slice().len() {
        return Err(VmError::Backend(format!(
            "vm-kvm: set_msrs wrote {written} of {} MSRs",
            msrs.as_slice().len(),
        )));
    }
    Ok(())
}

/// Restore the XSAVE area from the captured `region` bytes.
fn restore_xsave(vcpu: &VcpuFd, bytes: &[u8]) -> VmResult<()> {
    let mut xsave = kvm_xsave::default();
    let want = std::mem::size_of_val(&xsave.region);
    if bytes.len() != want {
        return Err(VmError::Backend(format!(
            "vm-kvm: snapshot xsave region is {} bytes, expected {want}",
            bytes.len(),
        )));
    }
    // SAFETY: `bytes` is exactly the size of the fixed `region` array
    // (checked above); we overwrite that array's bytes in a live `kvm_xsave`.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), xsave.region.as_mut_ptr().cast::<u8>(), want);
    }
    // SAFETY: `set_xsave` is unsafe because `kvm_xsave` carries a flexible
    // array member; we pass a default-constructed struct with no extra FAM
    // entries, so the kernel reads only the fixed `region` we populated.
    unsafe { vcpu.set_xsave(&xsave) }.map_err(kvm_err("set_xsave"))?;
    Ok(())
}

/// The set of MSR indices to snapshot: the kernel's advertised list. KVM
/// only exposes MSRs userspace may read/write here, so this is the safe
/// superset to capture.
pub fn snapshotable_msr_indices(kvm: &Kvm) -> VmResult<Vec<u32>> {
    let list = kvm
        .get_msr_index_list()
        .map_err(kvm_err("get_msr_index_list"))?;
    Ok(list.as_slice().to_vec())
}

/// Build a closure that wraps a kvm-ioctls error with the failing op name.
fn kvm_err(op: &'static str) -> impl Fn(kvm_ioctls::Error) -> VmError {
    move |e| VmError::Backend(format!("vm-kvm: {op}: {e}"))
}

/// Copy a POD `#[repr(C)]` KVM struct to its raw bytes.
pub(crate) fn pod_to_bytes<T: Copy>(value: &T) -> Vec<u8> {
    // SAFETY: `T` is a `#[repr(C)]` KVM binding struct — plain old data with
    // no pointers or padding invariants. We read exactly `size_of::<T>()`
    // bytes from a valid `&T`, producing an owned copy.
    let bytes = unsafe {
        std::slice::from_raw_parts((value as *const T).cast::<u8>(), std::mem::size_of::<T>())
    };
    bytes.to_vec()
}

/// Reconstruct a POD `#[repr(C)]` KVM struct from raw bytes, checking the
/// length matches the current struct layout.
pub(crate) fn pod_from_bytes<T: Copy + Default>(bytes: &[u8], what: &str) -> VmResult<T> {
    let want = std::mem::size_of::<T>();
    if bytes.len() != want {
        return Err(VmError::Backend(format!(
            "vm-kvm: snapshot {what} blob is {} bytes, expected {want} \
             (struct layout changed between capture and restore?)",
            bytes.len(),
        )));
    }
    let mut value = T::default();
    // SAFETY: `T` is POD; `bytes` is exactly `size_of::<T>()` long (checked
    // above) and `&mut value` points at a live, properly aligned `T`. We
    // overwrite all of its bytes with the captured copy.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), (&mut value as *mut T).cast::<u8>(), want);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A stand-in POD struct: the helpers are layout-agnostic, so testing
    // them on a plain repr(C) struct exercises the same bytes path the KVM
    // structs take — without needing /dev/kvm in CI.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    #[repr(C)]
    struct Sample {
        a: u64,
        b: u32,
        c: [u8; 8],
    }

    #[test]
    fn pod_round_trips_through_bytes() {
        let s = Sample {
            a: 0x0102_0304_0506_0708,
            b: 0xdead_beef,
            c: *b"NANOVM!!",
        };
        let bytes = pod_to_bytes(&s);
        assert_eq!(bytes.len(), std::mem::size_of::<Sample>());
        let back: Sample = pod_from_bytes(&bytes, "sample").unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn pod_from_bytes_rejects_wrong_length() {
        let err = pod_from_bytes::<Sample>(&[0u8; 4], "sample").unwrap_err();
        assert!(matches!(err, VmError::Backend(_)));
    }
}
