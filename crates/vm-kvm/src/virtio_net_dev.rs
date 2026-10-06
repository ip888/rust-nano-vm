//! # vm-kvm ↔ virtio-net glue
//!
//! > **Terminology:** every term here (virtio, virtqueue, GuestMemory,
//! > MMIO, orphan rule, newtype wrapper, …) is defined in the
//! > terminology table at the top of the `virtio-net` crate root.
//! > See `crates/virtio-net/src/lib.rs`.
//!
//! This module is the **plumbing between our KVM backend and the
//! virtio-net device**. It is split into small pieces that land across
//! several commits of sub-PR #C:
//!
//! | Commit | What it adds | Status |
//! |---|---|---|
//! | **#1 (this file today)** | `KvmNetGuestMemory` adapter — bridge from `vm_memory::GuestMemoryMmap` to `virtio_queue::GuestMemory` | ✅ this commit |
//! | #2 | MMIO region registration (give the device a 4 KiB window at `VIRTIO_NET_MMIO_BASE`) | Next |
//! | #3 | MMIO exit routing in the vCPU loop (`VcpuExit::MmioRead/Write` dispatch) | Next |
//! | #4 | Guest kernel cmdline + IRQ plumbing (`virtio_mmio.device=4K@0xd0000000:5`, `KVM_IRQ_LINE`) | Next |
//! | #5 | Real-KVM integration test (`virtio_net_boot.rs`) that boots a guest and asserts the device probes | Next |
//!
//! ## Why the newtype wrapper
//!
//! Both the trait we want to implement (`virtio_queue::GuestMemory`,
//! from the `virtio-queue` crate) and the type we want to implement
//! it for (`vm_memory::GuestMemoryMmap`, from the `vm-memory` crate)
//! are **foreign to this crate**. Rust's **orphan rule** forbids
//! `impl ForeignTrait for ForeignType` because two crates could then
//! each provide their own impl and the compiler wouldn't know which
//! to pick (a "coherence conflict").
//!
//! The standard escape is a **newtype wrapper**: wrap the foreign
//! type in our own `struct KvmNetGuestMemory<'a>(&'a GuestMemoryMmap)`
//! and implement the trait on *that*. Now the left-hand side (our
//! type) is local, which the orphan rule allows. Zero runtime cost —
//! `KvmNetGuestMemory` is just a `&GuestMemoryMmap` reference at
//! runtime.
//!
//! The same pattern is used three lines up for the virtio-vsock
//! device — see `GuestRamMem` in `src/lib.rs`. This file keeps the
//! net adapter separate rather than cramming more types into the
//! already-large `lib.rs`.
//!
//! ## What the adapter does
//!
//! Translates one trait's `read` / `write` methods into the other
//! trait's `read_slice` / `write_slice` methods. The underlying
//! `GuestMemoryMmap` already handles:
//!
//! - bounds checking against the guest's RAM map,
//! - overflow checking on `addr + len`,
//! - per-region dispatch (guest RAM can be non-contiguous),
//!
//! so our adapter is a one-liner delegate per method. Any error from
//! `vm-memory` becomes a `virtio_queue::QueueError::GuestMemoryOutOfBounds`
//! on our side.

#![cfg(feature = "kvm")]

use std::sync::{Arc, Mutex};

use kvm_ioctls::VmFd;
use virtio_net::{MockBackend, NetworkBackend, QueueCursor, VirtioNetDevice};
use virtio_queue::{GuestMemory, QueueError};
use vm_core::{VmError, VmResult};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

// --------------------------------------------------------------------------
// MMIO window + IRQ allocation
// --------------------------------------------------------------------------
//
// Our KVM backend already reserves 0xd000_0000..+0x1000 for the
// virtio-vsock device (see `VSOCK_MMIO_BASE` in `lib.rs`). virtio-net
// takes the next 4 KiB slot and the next free IRQ line.
//
// Why 0xd000_0000+? x86 microVM convention: guest kernel's "high MMIO
// hole" sits in the 3-4 GiB window (0xc000_0000..0xffff_ffff), above
// conventional RAM and below the APIC region. Firecracker, Cloud
// Hypervisor, QEMU-microvm all place virtio-mmio devices in this
// hole. Our vm-kvm memory map does not back these addresses with
// guest RAM — every guest access faults out as `KVM_EXIT_MMIO` and
// returns to us.
//
// 4 KiB (0x1000) is a comfortable window: the virtio-MMIO register
// bank is <0x200 bytes, so one 4-KiB page per device trivially
// contains it and leaves room for future config-space growth.

/// Guest-physical base address of the virtio-net MMIO register window.
/// Immediately after the virtio-vsock window.
pub(crate) const NET_MMIO_BASE: u64 = 0xd000_1000;

/// Size of the virtio-net MMIO window in bytes. One 4 KiB page.
pub(crate) const NET_MMIO_SIZE: u64 = 0x1000;

/// IRQ line the virtio-net device raises to notify the guest driver
/// that it has finished a virtqueue buffer. Next free line after
/// vsock's IRQ 5. The guest kernel command line references this
/// number via the `virtio_mmio.device=4K@0xd0001000:6` directive
/// that commit #4 of this sub-PR appends.
pub(crate) const NET_MMIO_IRQ: u32 = 6;

/// Adapter that lets the virtio-net queue consumer
/// ([`virtio_net::process_tx`] / [`virtio_net::process_rx`]) read and
/// write our KVM-managed guest RAM.
///
/// Carries a shared reference to the underlying `GuestMemoryMmap` — no
/// allocation, no `Box`, no `Arc`. Lifetime `'a` ties the adapter's
/// usefulness to the backing memory map.
///
/// # Rust concept: zero-sized indirection
///
/// At runtime a `KvmNetGuestMemory` is exactly the same size and
/// layout as the `&GuestMemoryMmap` it wraps — the newtype is a
/// compile-time distinction only. The compiler enforces that we can't
/// accidentally pass a raw `&GuestMemoryMmap` where a
/// `KvmNetGuestMemory` is expected (good — different semantics), but
/// generates identical machine code for the wrapper's method calls.
pub(crate) struct KvmNetGuestMemory<'a>(pub(crate) &'a GuestMemoryMmap);

impl GuestMemory for KvmNetGuestMemory<'_> {
    /// Copy `dst.len()` bytes from guest-physical address `addr` into
    /// `dst`. Delegates to `vm_memory`, which already bounds-checks
    /// against the configured guest RAM map.
    fn read(&self, addr: u64, dst: &mut [u8]) -> Result<(), QueueError> {
        // `read_slice` returns `vm_memory::Error` — we flatten it to
        // our trait's richer error variant. The specific loss here
        // (we don't propagate `vm_memory`'s error kind) is fine
        // because QueueError::GuestMemoryOutOfBounds is the only
        // outcome a buggy guest can trigger — bad address or length
        // past the end of guest RAM.
        self.0
            .read_slice(dst, GuestAddress(addr))
            .map_err(|_| QueueError::GuestMemoryOutOfBounds {
                addr,
                len: u32::try_from(dst.len()).unwrap_or(u32::MAX),
                mem_start: 0,
                mem_len: 0,
            })
    }

    /// Copy `src.len()` bytes from `src` into guest-physical address
    /// `addr`. Mirror of `read`.
    ///
    /// Note: the trait signature takes `&mut self`, but
    /// `GuestMemoryMmap::write_slice` only needs `&self` (the
    /// underlying mmap handles synchronisation). The `&mut` on our
    /// receiver is a trait-level artifact — we don't actually mutate
    /// anything inside the wrapper. Taking a shared `&self` here
    /// wouldn't work because the trait fixes the signature.
    fn write(&mut self, addr: u64, src: &[u8]) -> Result<(), QueueError> {
        self.0.write_slice(src, GuestAddress(addr)).map_err(|_| {
            QueueError::GuestMemoryOutOfBounds {
                addr,
                len: u32::try_from(src.len()).unwrap_or(u32::MAX),
                mem_start: 0,
                mem_len: 0,
            }
        })
    }
}

/// If `addr` falls inside the virtio-net device's MMIO register
/// window, return the offset within the window; otherwise `None`.
/// Lets the vCPU exit handler dispatch each MMIO exit to the right
/// device without needing a `NetBackend` reference (useful for the
/// case where the VM has no net device configured).
///
/// Free function rather than a method so unit tests can exercise it
/// without needing a `VmFd` (which requires `/dev/kvm`).
pub(crate) fn window_offset(addr: u64) -> Option<u64> {
    let off = addr.checked_sub(NET_MMIO_BASE)?;
    (off < NET_MMIO_SIZE).then_some(off)
}

// --------------------------------------------------------------------------
// NetBackend — shared handle on the virtio-net device (commit 2/5)
// --------------------------------------------------------------------------
//
// Mirrors the layout of `VsockBackend` in `lib.rs`: a device object
// behind `Arc<Mutex<_>>` so the vCPU thread (which services MMIO
// exits) and the backend-poll thread (which pulls RX frames from the
// network) can both reach it safely. `guest_mem` and `vm_fd` are
// cheaply cloneable handles the device-cycle paths need.
//
// What this commit sets up but does NOT yet wire:
//
// - the vCPU exit routing that dispatches `VcpuExit::MmioRead/Write`
//   through `window_offset` + `read` / `write` (commit 3/5),
// - the guest kernel cmdline + IRQ raising on virtqueue completion
//   (commit 4/5),
// - the real-KVM integration test that boots a guest and asserts
//   the driver probes the device (commit 5/5).
//
// So the public methods here are temporarily `#[allow(dead_code)]`.
// The exception goes away mechanically when commit 3 wires them up.

/// Shared handle on the host side of a virtio-net device, held by
/// both the hypervisor (for status queries / snapshot capture) and
/// the vCPU thread (for MMIO exit routing + IRQ injection).
///
/// # Rust concept: `Arc<Mutex<T>>` composition
///
/// - **`Arc<T>`** — Atomic Reference Counted shared pointer. Clones
///   are cheap (`Arc::clone(&self.device)` bumps a counter); the
///   underlying object is dropped when the last `Arc` goes away.
///   Thread-safe (atomic counter), unlike the single-thread `Rc`.
/// - **`Mutex<T>`** — mutual exclusion lock. Only one thread holds
///   `&mut T` at a time via `.lock()`; others block. Compile-time
///   guarantee replaces the usual runtime-only Java `synchronized`.
/// - **Composition `Arc<Mutex<T>>`** — "many owners, one at a time
///   mutates". The pattern Rust uses whenever a mutable resource
///   crosses thread boundaries without a dedicated owner.
///
/// We clone the whole `NetBackend` (its three `Arc`s and the
/// `GuestMemoryMmap` handle) into the vCPU thread at VM startup.
/// Both sides now have equally-valid handles; the Mutex arbitrates
/// mutation.
#[derive(Debug, Clone)]
pub(crate) struct NetBackend {
    /// The virtio-net device state (register bank, queue config,
    /// negotiated features). Locked for every MMIO exit and every
    /// virtqueue drain. Lock contention is low: a guest driver sees
    /// a few hundred MMIO exits during probe + feature negotiation,
    /// then only `QueueNotify` writes per packet — virtqueue drains
    /// are the hot path and they don't nest.
    device: Arc<Mutex<VirtioNetDevice>>,
    /// Per-queue cursors (`last_seen_avail` for TX and RX). The
    /// virtio-net queue consumer carries this state between calls to
    /// `process_tx` / `process_rx`. Stored behind a `Mutex` because
    /// both queue-drain threads (TX from MMIO kick, RX from backend
    /// readiness) may touch them.
    cursors: Arc<Mutex<NetQueueCursors>>,
    /// Shared handle on the guest's physical memory map. Cheap to
    /// clone — `GuestMemoryMmap` is internally refcounted. The queue
    /// consumer wraps this in `KvmNetGuestMemory` per call to satisfy
    /// the `virtio_queue::GuestMemory` trait.
    guest_mem: GuestMemoryMmap,
    /// Shared handle on the VM file descriptor; needed to raise the
    /// device's IRQ line (`KVM_IRQ_LINE` via `VmFd::set_irq_line`).
    /// Commit #4 of this sub-PR actually calls this; today the field
    /// is parked.
    vm_fd: Arc<VmFd>,
    /// The IRQ line this device raises. Mirrors [`NET_MMIO_IRQ`]
    /// but kept as an instance field so later changes to use a
    /// dynamic IRQ allocator don't ripple through callers.
    irq: u32,
}

/// Per-queue cursor state the virtio-net consumer carries between
/// `process_tx` / `process_rx` calls. Separate struct so the
/// `NetBackend` can own one `Mutex` protecting both cursors instead
/// of two — the two queues are drained from the same thread today,
/// so finer-grained locking would be overhead without benefit.
#[derive(Debug, Default)]
pub(crate) struct NetQueueCursors {
    pub tx: QueueCursor,
    pub rx: QueueCursor,
}

impl NetBackend {
    /// Construct a NetBackend around `device_backend` with the given
    /// MAC address. The MAC is advertised to the guest driver via
    /// the virtio-net config-space.
    ///
    /// `guest_mem` and `vm_fd` are stored as cheap-to-clone handles;
    /// future queue drains + IRQ raises use them without extra setup.
    pub fn new(
        mac: [u8; 6],
        device_backend: Arc<dyn NetworkBackend>,
        guest_mem: GuestMemoryMmap,
        vm_fd: Arc<VmFd>,
    ) -> Self {
        let device = VirtioNetDevice::with_backend_arc(device_backend, mac);
        Self {
            device: Arc::new(Mutex::new(device)),
            cursors: Arc::new(Mutex::new(NetQueueCursors::default())),
            guest_mem,
            vm_fd,
            irq: NET_MMIO_IRQ,
        }
    }

    /// Convenience constructor that wires a [`MockBackend`] as the
    /// transport. Used before sub-PR #D lands the smoltcp backend so
    /// that integration tests can exercise the KVM wiring end-to-end
    /// without a real network stack.
    pub fn with_mock_backend(mac: [u8; 6], guest_mem: GuestMemoryMmap, vm_fd: Arc<VmFd>) -> Self {
        Self::new(
            mac,
            Arc::new(MockBackend::new()) as Arc<dyn NetworkBackend>,
            guest_mem,
            vm_fd,
        )
    }

    /// Service a guest MMIO read inside the register window. Writes
    /// the little-endian value into `data` (zero-extending if the
    /// device returned fewer bytes than requested).
    pub fn read(&self, offset: u64, data: &mut [u8]) {
        let val = self
            .device
            .lock()
            .expect("vm-kvm: virtio-net device mutex poisoned")
            .mmio_read(offset, data.len());
        let bytes = val.to_le_bytes();
        for (i, slot) in data.iter_mut().enumerate() {
            *slot = bytes.get(i).copied().unwrap_or(0);
        }
    }

    /// Service a guest MMIO write inside the register window.
    /// Returns `Some(queue_idx)` when the write was a `QueueNotify`
    /// kick — the caller then drains that queue (commit #3 wires
    /// this). A write that only mutates feature-negotiation /
    /// config-space state returns `None`.
    pub fn write(&self, offset: u64, data: &[u8]) -> Option<u32> {
        let mut buf = [0u8; 8];
        for (i, b) in data.iter().take(buf.len()).enumerate() {
            buf[i] = *b;
        }
        let value = u64::from_le_bytes(buf);
        self.device
            .lock()
            .expect("vm-kvm: virtio-net device mutex poisoned")
            .mmio_write(offset, data.len(), value)
    }

    /// Drain the queue the guest just kicked (`queue_idx` from a
    /// `QueueNotify` MMIO write). Walks every descriptor chain the
    /// guest driver has published since the cursor's last run,
    /// hands the Ethernet frames to the backend (`process_tx`) or
    /// fills guest buffers with backend-produced frames
    /// (`process_rx`), returns the chain heads to the used ring,
    /// and — if any chain completed — raises the device IRQ.
    ///
    /// The lock order here matters:
    ///
    /// 1. Lock the device to snapshot the queue config + clone the
    ///    backend Arc (both cheap). Release.
    /// 2. Lock the cursors mutex (also cheap).
    /// 3. Call `process_tx` or `process_rx` with the shared
    ///    backend handle. Backend calls may do blocking syscalls
    ///    (TAP write / smoltcp socket read), which is why step 1
    ///    released the device lock before this.
    /// 4. If stats say we should raise, re-lock the device to flip
    ///    the VRING bit in the transport's InterruptStatus register.
    /// 5. Pulse the KVM IRQ line (level-triggered: assert, deassert).
    ///
    /// Unknown `queue_idx` → no-op. The guest driver only has two
    /// legitimate queues; anything else is a buggy or malicious
    /// kick we silently drop.
    pub fn drain_queue(&self, queue_idx: u32) -> VmResult<()> {
        use virtio_net::{process_rx, process_tx, RX_QUEUE_INDEX, TX_QUEUE_INDEX};

        // --- Step 1: snapshot inside device lock, then release ------
        let (queue_cfg, backend) = {
            let device = self
                .device
                .lock()
                .expect("vm-kvm: virtio-net device mutex poisoned");
            // Both queues at known indices (0=RX, 1=TX from mmio.rs).
            // Reject anything else without taking the backend ref.
            let Some(cfg) = device.transport().queue(queue_idx as usize) else {
                return Ok(());
            };
            (*cfg, device.backend_arc())
        };

        if queue_idx != TX_QUEUE_INDEX && queue_idx != RX_QUEUE_INDEX {
            return Ok(());
        }

        // --- Step 2 + 3: drain outside the device lock -------------
        let mut cursors = self
            .cursors
            .lock()
            .expect("vm-kvm: virtio-net cursors mutex poisoned");
        let mut mem = KvmNetGuestMemory(&self.guest_mem);

        let stats = if queue_idx == TX_QUEUE_INDEX {
            process_tx(&queue_cfg, &mut cursors.tx, &mut mem, &*backend)
                .map_err(|e| VmError::Backend(format!("virtio-net process_tx: {e}")))?
        } else {
            // RX_QUEUE_INDEX
            process_rx(&queue_cfg, &mut cursors.rx, &mut mem, &*backend)
                .map_err(|e| VmError::Backend(format!("virtio-net process_rx: {e}")))?
        };
        drop(cursors);

        // --- Step 4: re-lock device to flip InterruptStatus bit ----
        if stats.should_raise_interrupt() {
            self.device
                .lock()
                .expect("vm-kvm: virtio-net device mutex poisoned")
                .raise_vring_interrupt();

            // --- Step 5: pulse the KVM IRQ line (level-triggered) --
            // Assert, then deassert — the KVM edge triggers the IDT
            // vector; the guest driver reads InterruptStatus to
            // discover which bit was set and ACKs by writing
            // InterruptACK (handled by MmioTransport::write).
            self.vm_fd.set_irq_line(self.irq, true).map_err(|e| {
                VmError::Backend(format!("assert virtio-net IRQ {}: {e}", self.irq))
            })?;
            self.vm_fd.set_irq_line(self.irq, false).map_err(|e| {
                VmError::Backend(format!("deassert virtio-net IRQ {}: {e}", self.irq))
            })?;
        }
        Ok(())
    }

    /// Current virtio-net device status register as the guest sees
    /// it: `ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK` bits set
    /// as the guest driver walks the bring-up. Used by the host-side
    /// `KvmHypervisor::net_status` accessor and by integration tests
    /// that poll for guest readiness.
    pub fn status(&self) -> u32 {
        self.device
            .lock()
            .expect("vm-kvm: virtio-net device mutex poisoned")
            .status()
    }

    /// `true` once the guest driver has finished bringing up the
    /// device (set `DRIVER_OK` after feature negotiation + queue
    /// setup). The integration test polls this to confirm the full
    /// virtio probe sequence succeeded.
    pub fn driver_ok(&self) -> bool {
        self.device
            .lock()
            .expect("vm-kvm: virtio-net device mutex poisoned")
            .driver_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestMemoryMmap;

    /// Build a `GuestMemoryMmap` with one region of `size` bytes
    /// starting at `base`. Enough fidelity for an adapter unit test —
    /// we don't need a real KVM here, just a memory map we can read
    /// and write.
    fn tiny_mem(base: u64, size: usize) -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(base), size)]).expect("build GuestMemoryMmap")
    }

    #[test]
    fn adapter_round_trips_bytes_at_zero_base() {
        let mem = tiny_mem(0, 4096);
        let adapter = KvmNetGuestMemory(&mem);

        // Pre-fill through the raw vm-memory API.
        mem.write_slice(&[1, 2, 3, 4, 5], GuestAddress(0x10))
            .unwrap();

        let mut buf = [0u8; 5];
        adapter.read(0x10, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn adapter_round_trips_bytes_at_non_zero_base() {
        // Guest RAM typically starts above the low-memory window
        // reserved for BIOS / MMIO. Simulate that layout.
        let base: u64 = 0x8000_0000;
        let mem = tiny_mem(base, 4096);
        let mut adapter = KvmNetGuestMemory(&mem);

        adapter.write(base + 0x20, &[0xAA, 0xBB, 0xCC]).unwrap();
        let mut buf = [0u8; 3];
        adapter.read(base + 0x20, &mut buf).unwrap();
        assert_eq!(buf, [0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn adapter_rejects_read_past_the_end() {
        let mem = tiny_mem(0, 1024);
        let adapter = KvmNetGuestMemory(&mem);

        let mut buf = [0u8; 16];
        // Last valid read is at [1008, 1024). 1016 + 16 = 1032 > 1024.
        let err = adapter.read(1016, &mut buf).unwrap_err();
        assert!(
            matches!(err, QueueError::GuestMemoryOutOfBounds { addr: 1016, .. }),
            "expected OOB on overrun, got {err:?}",
        );
    }

    #[test]
    fn adapter_rejects_write_past_the_end() {
        let mem = tiny_mem(0, 1024);
        let mut adapter = KvmNetGuestMemory(&mem);
        let err = adapter.write(1016, &[0u8; 16]).unwrap_err();
        assert!(matches!(err, QueueError::GuestMemoryOutOfBounds { .. }));
    }

    // ----------------------------------------------------------------
    // Constants + window_offset (commit 2 scope)
    // ----------------------------------------------------------------

    // Compile-time invariants on the MMIO constants. These are
    // `const _:` items rather than runtime `#[test]`s because every
    // comparison is between literals — clippy's
    // `assertions_on_constants` rule rightly prefers the compile-time
    // form. A violation turns into a build error, not a test failure,
    // which is a strictly better outcome.
    const _: () = {
        // vsock lives at 0xd000_0000..+0x1000. We must not overlap it.
        const VSOCK_BASE: u64 = 0xd000_0000;
        const VSOCK_END: u64 = VSOCK_BASE + 0x1000;
        assert!(NET_MMIO_BASE >= VSOCK_END, "net overlaps vsock window");
        assert!(NET_MMIO_SIZE > 0);
        // virtio-mmio spec requires a page-aligned register window.
        assert!(
            NET_MMIO_BASE.is_multiple_of(0x1000),
            "net base must be page-aligned"
        );
        assert!(
            NET_MMIO_SIZE.is_multiple_of(0x1000),
            "net size must be page-aligned"
        );
        // IRQ must not collide with vsock (IRQ 5).
        assert!(NET_MMIO_IRQ != 5, "net IRQ collides with vsock IRQ");
    };

    #[test]
    fn window_offset_accepts_addresses_inside_window() {
        // First byte of the window → offset 0.
        assert_eq!(window_offset(NET_MMIO_BASE), Some(0));
        // Last byte of the window → offset SIZE-1.
        assert_eq!(
            window_offset(NET_MMIO_BASE + NET_MMIO_SIZE - 1),
            Some(NET_MMIO_SIZE - 1)
        );
    }

    #[test]
    fn window_offset_rejects_addresses_below_window() {
        assert_eq!(window_offset(NET_MMIO_BASE - 1), None);
        assert_eq!(window_offset(0), None);
    }

    #[test]
    fn window_offset_rejects_addresses_past_window_end() {
        assert_eq!(window_offset(NET_MMIO_BASE + NET_MMIO_SIZE), None);
        assert_eq!(window_offset(NET_MMIO_BASE + NET_MMIO_SIZE + 100), None);
    }

    #[test]
    fn adapter_handles_virtio_queue_descriptor_sized_bursts() {
        // The real queue consumer does 16-byte reads for descriptor
        // table entries and 2-byte reads for ring indices. Sanity-check
        // those access sizes behave identically.
        let mem = tiny_mem(0, 4096);
        let adapter = KvmNetGuestMemory(&mem);
        mem.write_slice(&[0xDE; 16], GuestAddress(0x100)).unwrap();
        let mut sixteen = [0u8; 16];
        adapter.read(0x100, &mut sixteen).unwrap();
        assert_eq!(sixteen, [0xDE; 16]);
        mem.write_slice(&[0xBE, 0xEF], GuestAddress(0x200)).unwrap();
        let mut two = [0u8; 2];
        adapter.read(0x200, &mut two).unwrap();
        assert_eq!(two, [0xBE, 0xEF]);
    }
}
