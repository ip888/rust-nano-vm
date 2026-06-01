# Faithful KVM snapshot/restore in <1000 lines of Rust

> Companion to [post #1: `MAP_PRIVATE` is the whole trick](01-mmap-private.md).
> That post explained how rust-nano-vm gets ~12 ms cold starts and
> ~0.5 MiB per-fork memory by mapping the snapshot RAM `MAP_PRIVATE`.
> This post is the other half: how we actually capture and restore
> enough KVM state for the guest to *resume*, not just re-boot.

## The state you have to capture

A running KVM vCPU is more than its general-purpose registers. To
resume exactly where you froze, you need:

| State | KVM struct | Why it matters |
| --- | --- | --- |
| GPRs + RIP + RFLAGS | `kvm_regs` | program counter, stack |
| Segments + CR0..4 + EFER | `kvm_sregs` | mode, paging, GDT/IDT |
| FPU / SSE | `kvm_fpu` | x87 register file |
| Extended FPU / AVX | `kvm_xsave` | XSAVE area |
| XCR0 | `kvm_xcrs` | which XSAVE components are live |
| DR0..7 | `kvm_debugregs` | breakpoints |
| Run / halt state | `kvm_mp_state` | INIT / SIPI / RUNNABLE |
| Pending exceptions/IRQs | `kvm_vcpu_events` | injection state |
| Local APIC | `kvm_lapic_state` | timers, IPIs |
| Subset of MSRs | `Msrs` | TSC, KERNEL_GS_BASE, etc |

And the **VM-level** in-kernel devices:

| State | KVM struct |
| --- | --- |
| 8259 PIC master | `kvm_irqchip` |
| 8259 PIC slave  | `kvm_irqchip` |
| IOAPIC | `kvm_irqchip` |
| 8254 PIT | `kvm_pit_state2` |
| KVM paravirtual clock | `kvm_clock_data` |

Miss any one of these and the guest looks fine for a few hundred
microseconds, then derails — typically wedging in `lapic_timer_fn` or
returning to a half-saved interrupt frame.

## The on-disk layout

We split the snapshot into three pieces:

```
snap-0000000000000001/
  manifest.json     -- portable: VM ID, memory size, page size, version
  memory.bin        -- raw RAM image; mmap(MAP_PRIVATE)-able
  vcpu0.json        -- VcpuState (this post)
  machine.json      -- MachineState (this post)
```

The split matters because **`memory.bin` is the only thing that has to
be page-aligned and `mmap`-able directly**. Everything else is small
metadata; JSON is fine for it and survives format changes via serde's
`#[serde(default)]`. The `kvm-bindings` crate ships a `serde` feature
that derives `Serialize` / `Deserialize` on the C structs — but we
store the raw `#[repr(C)]` bytes instead, because the KVM ABI is
defined in those bytes and a byte-for-byte capture is the only thing
that's guaranteed to round-trip across crate version bumps.

## The capture path

Each KVM struct goes in as its raw bytes
([`crates/vm-kvm/src/vmstate.rs`](../../crates/vm-kvm/src/vmstate.rs)):

```rust
pub fn capture(vcpu: &VcpuFd, msr_indices: &[u32]) -> VmResult<Self> {
    Ok(Self {
        regs: pod_to_bytes(&vcpu.get_regs()?),
        sregs: pod_to_bytes(&vcpu.get_sregs()?),
        fpu: pod_to_bytes(&vcpu.get_fpu()?),
        xcrs: pod_to_bytes(&vcpu.get_xcrs()?),
        debug_regs: pod_to_bytes(&vcpu.get_debug_regs()?),
        mp_state: pod_to_bytes(&vcpu.get_mp_state()?),
        vcpu_events: pod_to_bytes(&vcpu.get_vcpu_events()?),
        lapic: pod_to_bytes(&vcpu.get_lapic()?),
        xsave: pod_to_bytes(&vcpu.get_xsave()?.region),
        msrs: capture_msrs(vcpu, msr_indices)?,
    })
}
```

`pod_to_bytes` is a tiny helper that returns a `Vec<u8>` of the
struct's size. We record the size on capture and check it on restore
so a struct-layout change between capture and restore is caught loudly
instead of silently corrupting vCPU state:

```rust
let bytes = pod_from_bytes::<kvm_regs>(&self.regs, "regs")?;
// pod_from_bytes errors if bytes.len() != size_of::<kvm_regs>()
//   "vm-kvm: regs blob is N bytes, expected M
//    (struct layout changed between capture and restore?)"
```

## The restore order matters

KVM's restore ioctls aren't independent. The order we use
(`vmstate.rs::restore`):

```
set_sregs        // before regs: page tables / CR3 affect register interpretation
set_regs         // GPRs + RIP
set_fpu          // legacy FPU
set_xcrs         // XCR0 *before* xsave so XSAVE knows which components are live
set_debug_regs
restore_msrs     // before LAPIC because TSC offsets affect timers
restore_xsave    // after XCRs
set_lapic        // after MSRs
set_vcpu_events  // pending IRQ/NMI/SMI, set last so injection state is fresh
set_mp_state     // run/halt last
```

Get this order wrong and the guest will *appear* to resume — the
process exists, KVM_RUN doesn't error — but the wrong things will be
injected, the wrong XSAVE components will be considered live, or the
wrong MSRs will be in effect when the LAPIC arms its next timer. The
symptom is "guest panics inside the next 10 ms".

## In-kernel device state

The VM-level state is shorter — five blobs:

```rust
impl MachineState {
    pub fn capture(vm: &VmFd) -> VmResult<Self> {
        Ok(Self {
            pic_master: capture_irqchip(vm, KVM_IRQCHIP_PIC_MASTER)?,
            pic_slave:  capture_irqchip(vm, KVM_IRQCHIP_PIC_SLAVE)?,
            ioapic:     capture_irqchip(vm, KVM_IRQCHIP_IOAPIC)?,
            pit:        pod_to_bytes(&vm.get_pit2()?),
            clock:      pod_to_bytes(&vm.get_clock()?),
        })
    }
    pub fn restore(&self, vm: &VmFd) -> VmResult<()> {
        restore_irqchip(vm, &self.pic_master, "pic_master")?;
        restore_irqchip(vm, &self.pic_slave,  "pic_slave")?;
        restore_irqchip(vm, &self.ioapic,     "ioapic")?;
        vm.set_pit2(&pod_from_bytes::<kvm_pit_state2>(&self.pit, "pit")?)?;
        vm.set_clock(&pod_from_bytes::<kvm_clock_data>(&self.clock, "clock")?)?;
        Ok(())
    }
}
```

`kvm_irqchip` is a tagged union; the `chip_id` field at the top
selects which controller's state lives in the rest of the bytes. We
capture all three so the restored guest's interrupt routing is
identical to the captured one.

## What we deliberately *don't* do

- **No cross-kernel migration.** A snapshot is a same-host,
  same-kernel artifact. The KVM struct layouts are stable within a
  major kernel version but not promised across versions. Cross-host
  migration would need a versioned, field-wise schema; we don't
  promise that.
- **No live migration.** No pre-copy, no post-copy, no dirty-page
  tracking. The use case is "snapshot once, fork many", not "move
  workload between hosts".
- **No userfaultfd.** Originally planned in M5; turned out
  `mmap(MAP_PRIVATE)` is enough for sub-15 ms cold starts on its own.
  See [post #1](01-mmap-private.md).

## Line count

```sh
$ wc -l crates/vm-kvm/src/vmstate.rs crates/snapshot/src/lib.rs
  366 crates/vm-kvm/src/vmstate.rs
  850 crates/snapshot/src/lib.rs
```

The portable on-disk format (manifest, page header, version checks,
glob-listing) lives in [`crates/snapshot`](../../crates/snapshot/);
the KVM-specific capture/restore lives in
[`crates/vm-kvm/src/vmstate.rs`](../../crates/vm-kvm/src/vmstate.rs).
Together: under 1200 lines, including doc comments and tests.

## Reproduce

Three end-to-end tests, all behind the `kvm` feature:

```sh
# Single VM, full snapshot/restore round-trip:
cargo test -p vm-kvm --features kvm --test snapshot_restore_boot

# Fan-out: snapshot once, fork 50, all run to completion:
cargo test -p vm-kvm --features kvm --test snapshot_fork_many_boot

# vsock guest-agent path through a restored guest:
cargo test -p vm-kvm --features kvm --test vsock_driver_ok_boot
```

---

Previous post: [How rust-nano-vm cold-starts in ~12 ms](01-mmap-private.md).

Code: https://github.com/ip888/Rust-nano-vm
