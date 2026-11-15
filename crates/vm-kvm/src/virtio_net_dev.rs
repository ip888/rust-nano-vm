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

use std::sync::{Arc, Mutex, MutexGuard};
#[cfg(feature = "virtio-net-smoltcp")]
use std::{
    sync::Condvar,
    thread::{self, JoinHandle},
};

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
    /// Serializes MMIO writes that can replace or disable a queue
    /// configuration with the full drain operation, including used
    /// ring updates and interrupt delivery.
    drain_gate: Arc<Mutex<()>>,
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

fn lock_drain_gate(drain_gate: &Mutex<()>) -> MutexGuard<'_, ()> {
    drain_gate
        .lock()
        .expect("vm-kvm: virtio-net drain mutex poisoned")
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
            drain_gate: Arc::new(Mutex::new(())),
            cursors: Arc::new(Mutex::new(NetQueueCursors::default())),
            guest_mem,
            vm_fd,
            irq: NET_MMIO_IRQ,
        }
    }

    /// Convenience constructor that wires a [`MockBackend`] as the
    /// transport. Frames the guest emits are discarded; `read_frame`
    /// never returns anything. Used by every virtio bring-up test
    /// that only cares about handshake (feature negotiation + queue
    /// setup + DRIVER_OK), not actual packet flow.
    pub fn with_mock_backend(mac: [u8; 6], guest_mem: GuestMemoryMmap, vm_fd: Arc<VmFd>) -> Self {
        Self::new(
            mac,
            Arc::new(MockBackend::new()) as Arc<dyn NetworkBackend>,
            guest_mem,
            vm_fd,
        )
    }

    /// Convenience constructor that wires a userspace smoltcp TCP/IP
    /// stack as the transport. The resulting NetBackend lets the
    /// host-side `TcpProxy` from `virtio_net::tcp_proxy` dial into
    /// servers running inside the guest (Spring Boot on :8080, etc.)
    /// through nothing but process-local memory — no TAP device, no
    /// `CAP_NET_ADMIN`, works in a stock Fly.io machine.
    ///
    /// Gated behind the crate's `virtio-net-smoltcp` cargo feature so
    /// bare `--features kvm` builds don't drag the smoltcp dependency
    /// in. Also exposed through `KvmHypervisor::net_backend()` so
    /// callers can attach a proxy to the running VM.
    ///
    /// # Host-originated RX wake-up
    ///
    /// Returns a third handle — an [`Arc<NetWakeDriver>`] — on top
    /// of the usual `(NetBackend, SmoltcpBackend)` tuple. The
    /// driver owns a background thread that drains the guest-RX
    /// virtqueue whenever the smoltcp backend fires its
    /// `RxWaker`; without it, host-originated SYNs and reply
    /// frames would sit in `stack_to_guest` until the guest
    /// kicked for its own reasons. `KvmHypervisor` stores the
    /// `Arc` so the thread stays alive for the VM's lifetime
    /// and shuts down cleanly on `Drop`.
    ///
    /// Closes Copilot finding #4252252295 from PR #283, part 2/2
    /// (the backend-side `RxWaker` hook was PR #296).
    #[cfg(feature = "virtio-net-smoltcp")]
    pub fn with_smoltcp_backend(
        mac: [u8; 6],
        guest_mem: GuestMemoryMmap,
        vm_fd: Arc<VmFd>,
    ) -> (Self, Arc<virtio_net::SmoltcpBackend>, Arc<NetWakeDriver>) {
        // Shared coalescing state between the SmoltcpBackend's
        // RxWaker closure and the wake thread. A single boolean
        // pending-flag behind a Mutex + Condvar instead of an
        // unbounded channel: an idle host-side proxy polling
        // `socket_recv` fires the waker on every call (a few kHz
        // under light load), but the thread only needs *one* drain
        // per batch of frames. Coalescing keeps the backlog
        // bounded — the flag is at most "set" or "clear", never a
        // queue.
        let state = Arc::new(WakeState::new());

        // The RxWaker callback is invoked after every host-side
        // method on `SmoltcpBackend` (open_client_socket,
        // socket_send, socket_recv, close_socket, write_frame)
        // AFTER the backend's internal mutex has been released.
        // Signalling an already-pending flag is a cheap no-op:
        // the thread coalesces.
        let state_for_waker = Arc::clone(&state);
        let waker: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            state_for_waker.signal_rx_ready();
        });

        let smoltcp = Arc::new(
            virtio_net::smoltcp_backend::SmoltcpBackend::with_config_and_rx_waker(
                virtio_net::smoltcp_backend::BackendConfig::default(),
                waker,
            ),
        );
        let backend = Self::new(
            mac,
            Arc::clone(&smoltcp) as Arc<dyn NetworkBackend>,
            guest_mem,
            vm_fd,
        );

        // Spawn the wake thread with cloned Arcs (NOT a NetBackend
        // clone — a NetBackend clone would create a cycle if the
        // NetWakeDriver were held inside NetBackend. Keeping the
        // thread's state flat lets `KvmHypervisor` own the driver
        // separately, with no back-reference from NetBackend).
        let wake_driver = Arc::new(NetWakeDriver::spawn(
            state,
            Arc::clone(&backend.device),
            Arc::clone(&backend.drain_gate),
            Arc::clone(&backend.cursors),
            backend.guest_mem.clone(),
            Arc::clone(&backend.vm_fd),
            backend.irq,
        ));
        (backend, smoltcp, wake_driver)
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
        let _drain_gate = lock_drain_gate(&self.drain_gate);
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
    /// 1. Lock the drain gate, shared with MMIO writes. This prevents
    ///    queue disable/reset from returning while a drain uses the
    ///    old queue configuration.
    /// 2. Lock the device to snapshot the queue config + clone the
    ///    backend Arc (both cheap). Release.
    /// 3. Lock the cursors mutex (also cheap).
    /// 4. Call `process_tx` or `process_rx` with the shared
    ///    backend handle. Backend calls may do blocking syscalls
    ///    (TAP write / smoltcp socket read), which is why step 2
    ///    released the device lock before this.
    /// 5. If stats say we should raise, re-lock the device to flip
    ///    the VRING bit in the transport's InterruptStatus register.
    /// 6. Pulse the KVM IRQ line (level-triggered: assert, deassert).
    ///
    /// Unknown `queue_idx` → no-op. The guest driver only has two
    /// legitimate queues; anything else is a buggy or malicious
    /// kick we silently drop.
    pub fn drain_queue(&self, queue_idx: u32) -> VmResult<()> {
        Self::drain_queue_impl(
            &self.device,
            &self.drain_gate,
            &self.cursors,
            &self.guest_mem,
            &self.vm_fd,
            self.irq,
            queue_idx,
        )
    }

    /// Same drain logic as [`drain_queue`] but takes the shared
    /// handles (`device`, `drain_gate`, `cursors`, `guest_mem`,
    /// `vm_fd`) and the `irq` number by reference. This lets the
    /// smoltcp wake thread (see [`NetWakeDriver`]) drive RX drains
    /// without holding a full `NetBackend` clone, which would create
    /// a reference cycle via the [`KvmHypervisor::net_wake_driver`]
    /// field.
    pub(crate) fn drain_queue_impl(
        device: &Arc<Mutex<VirtioNetDevice>>,
        drain_gate: &Arc<Mutex<()>>,
        cursors: &Arc<Mutex<NetQueueCursors>>,
        guest_mem: &GuestMemoryMmap,
        vm_fd: &Arc<VmFd>,
        irq: u32,
        queue_idx: u32,
    ) -> VmResult<()> {
        use virtio_net::{process_rx, process_tx, RX_QUEUE_INDEX, TX_QUEUE_INDEX};

        let _drain_gate = lock_drain_gate(drain_gate);

        // --- Step 1: snapshot inside device lock, then release ------
        let (queue_cfg, backend) = {
            let device = device
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
        let mut cursors = cursors
            .lock()
            .expect("vm-kvm: virtio-net cursors mutex poisoned");
        let mut mem = KvmNetGuestMemory(guest_mem);

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
            device
                .lock()
                .expect("vm-kvm: virtio-net device mutex poisoned")
                .raise_vring_interrupt();

            // --- Step 5: pulse the KVM IRQ line (level-triggered) --
            // Assert, then deassert — the KVM edge triggers the IDT
            // vector; the guest driver reads InterruptStatus to
            // discover which bit was set and ACKs by writing
            // InterruptACK (handled by MmioTransport::write).
            vm_fd
                .set_irq_line(irq, true)
                .map_err(|e| VmError::Backend(format!("assert virtio-net IRQ {irq}: {e}")))?;
            vm_fd
                .set_irq_line(irq, false)
                .map_err(|e| VmError::Backend(format!("deassert virtio-net IRQ {irq}: {e}")))?;
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

/// Shared coalescing state between the SmoltcpBackend's `RxWaker`
/// closure (producer) and the wake thread (consumer). Bounded by
/// design: a single boolean pending-flag plus a shutdown flag,
/// both behind one Mutex, coordinated by a Condvar. Multiple
/// `signal_rx_ready` calls between thread iterations collapse to
/// one drain pass.
#[cfg(feature = "virtio-net-smoltcp")]
pub(crate) struct WakeState {
    inner: Mutex<WakeInner>,
    cv: Condvar,
}

#[cfg(feature = "virtio-net-smoltcp")]
struct WakeInner {
    /// True = at least one `signal_rx_ready` has fired since the
    /// thread last drained. Set by the waker closure, cleared by
    /// the thread when it picks up the work.
    rx_pending: bool,
    /// Set once by `signal_shutdown` to tell the thread to exit at
    /// its next wake. Never cleared.
    shutdown: bool,
}

#[cfg(feature = "virtio-net-smoltcp")]
impl WakeState {
    fn new() -> Self {
        Self {
            inner: Mutex::new(WakeInner {
                rx_pending: false,
                shutdown: false,
            }),
            cv: Condvar::new(),
        }
    }

    /// Mark RX work as pending and wake the thread. Coalescing: if
    /// the flag was already set, the extra call is a near-no-op —
    /// the thread still only drains once per wake pass.
    fn signal_rx_ready(&self) {
        let mut inner = self.inner.lock().expect("vm-kvm: WakeState mutex poisoned");
        inner.rx_pending = true;
        self.cv.notify_one();
    }

    /// Mark shutdown + wake. Independent of `rx_pending` so
    /// teardown is never blocked behind a backlog of RX work.
    fn signal_shutdown(&self) {
        let mut inner = self.inner.lock().expect("vm-kvm: WakeState mutex poisoned");
        inner.shutdown = true;
        self.cv.notify_one();
    }
}

/// RAII owner of the host-RX wake background thread.
///
/// Spawned by [`NetBackend::with_smoltcp_backend`] and stored inside
/// `KvmHypervisor::runtime.net_wake_driver` so the thread lives as
/// long as the VM. On `Drop` it signals shutdown through the shared
/// [`WakeState`] and joins the thread, so the VM teardown path
/// never leaks a thread per restored VM.
///
/// Deliberately **not** stored inside `NetBackend`: a NetBackend
/// clone handed to the wake thread (for `drain_queue` access) would
/// transitively hold back a strong reference to the NetWakeDriver,
/// creating a cycle that would keep the thread alive past the VM's
/// end. The thread instead holds flat clones of the four Arcs it
/// needs (`device`, `cursors`, `guest_mem`, `vm_fd`), none of which
/// transitively references the driver.
#[cfg(feature = "virtio-net-smoltcp")]
pub(crate) struct NetWakeDriver {
    /// Shared coalescing state — [`Drop`] uses it to signal
    /// shutdown independently of any pending RX work.
    state: Arc<WakeState>,
    /// `Option` so `Drop` can `.take()` the handle and join without
    /// moving out of `&mut self`.
    thread: Option<JoinHandle<()>>,
}

#[cfg(feature = "virtio-net-smoltcp")]
impl std::fmt::Debug for NetWakeDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetWakeDriver")
            .field("thread_alive", &self.thread.is_some())
            .finish()
    }
}

#[cfg(feature = "virtio-net-smoltcp")]
impl NetWakeDriver {
    /// Spawn the wake thread. The driver retains a clone of
    /// `state` so [`Drop`] can signal shutdown; the thread moves
    /// in its own clone.
    fn spawn(
        state: Arc<WakeState>,
        device: Arc<Mutex<VirtioNetDevice>>,
        drain_gate: Arc<Mutex<()>>,
        cursors: Arc<Mutex<NetQueueCursors>>,
        guest_mem: GuestMemoryMmap,
        vm_fd: Arc<VmFd>,
        irq: u32,
    ) -> Self {
        let state_for_thread = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name("nanovm-net-wake".into())
            .spawn(move || {
                run_wake_loop(&state_for_thread, move || {
                    // Errors from drain are not fatal — the guest
                    // may have torn down the queue mid-wake. The
                    // next guest-side QueueNotify kicks a fresh
                    // drain anyway.
                    let _ = NetBackend::drain_queue_impl(
                        &device,
                        &drain_gate,
                        &cursors,
                        &guest_mem,
                        &vm_fd,
                        irq,
                        virtio_net::RX_QUEUE_INDEX,
                    );
                });
            })
            .expect("vm-kvm: spawning nanovm-net-wake thread must not fail");
        Self {
            state,
            thread: Some(thread),
        }
    }
}

/// Pure wake-loop extracted so unit tests can exercise the event-
/// handling and shutdown semantics without needing a real `VmFd`
/// (which requires `/dev/kvm`). Production `NetWakeDriver::spawn`
/// passes a closure that invokes
/// [`NetBackend::drain_queue_impl`]; tests pass a counter-
/// incrementing closure to assert on wake-pass counts.
///
/// Exit paths:
/// - `shutdown` flag set via [`WakeState::signal_shutdown`]: clean
///   exit regardless of pending RX work.
/// - Mutex poisoned: panics on next `lock()` — the thread's own
///   drain is in trouble anyway.
#[cfg(feature = "virtio-net-smoltcp")]
fn run_wake_loop<F>(state: &WakeState, mut on_rx_ready: F)
where
    F: FnMut(),
{
    loop {
        // Scope the guard so it's released before the drain runs.
        // The drain acquires the device mutex; releasing the wake
        // state mutex here prevents the host-side waker closure
        // from piling up behind us while we drain.
        let drain_now = {
            let mut inner = state
                .inner
                .lock()
                .expect("vm-kvm: WakeState mutex poisoned");
            while !inner.rx_pending && !inner.shutdown {
                inner = state
                    .cv
                    .wait(inner)
                    .expect("vm-kvm: WakeState condvar poisoned");
            }
            if inner.shutdown {
                return;
            }
            // Clear the flag as we pick up the work. Any waker
            // fires that land AFTER this point will set it back to
            // true and cause the next loop iteration to drain
            // again — no event is lost, but concurrent fires during
            // the drain itself collapse to one follow-up pass.
            inner.rx_pending = false;
            true
        };
        if drain_now {
            on_rx_ready();
        }
    }
}

#[cfg(feature = "virtio-net-smoltcp")]
impl Drop for NetWakeDriver {
    fn drop(&mut self) {
        self.state.signal_shutdown();
        if let Some(handle) = self.thread.take() {
            // join() surfaces thread panics as Err. Swallow — the
            // VM is being torn down, nothing useful to do with it.
            let _ = handle.join();
        }
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
    fn queue_reset_waits_for_active_drain() {
        use std::sync::mpsc;

        let drain_gate = Arc::new(Mutex::new(()));
        let (drain_started_tx, drain_started_rx) = mpsc::channel();
        let (finish_drain_tx, finish_drain_rx) = mpsc::channel();
        let drain_gate_for_thread = Arc::clone(&drain_gate);
        let drain_thread = std::thread::spawn(move || {
            let _guard = lock_drain_gate(&drain_gate_for_thread);
            drain_started_tx.send(()).unwrap();
            finish_drain_rx.recv().unwrap();
        });

        drain_started_rx.recv().unwrap();
        let (reset_started_tx, reset_started_rx) = mpsc::channel();
        let (reset_done_tx, reset_done_rx) = mpsc::channel();
        let reset_gate = Arc::clone(&drain_gate);
        let reset_thread = std::thread::spawn(move || {
            reset_started_tx.send(()).unwrap();
            let _guard = lock_drain_gate(&reset_gate);
            reset_done_tx.send(()).unwrap();
        });

        reset_started_rx.recv().unwrap();
        assert!(reset_done_rx.try_recv().is_err());
        finish_drain_tx.send(()).unwrap();
        reset_done_rx.recv().unwrap();
        drain_thread.join().unwrap();
        reset_thread.join().unwrap();
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

    // ----------------------------------------------------------------
    // Host-RX wake loop (smoltcp-only)
    // ----------------------------------------------------------------
    //
    // These tests exercise [`run_wake_loop`] directly instead of
    // going through `NetWakeDriver::spawn`, because `spawn` needs a
    // real `VmFd` (which requires `/dev/kvm`) and these need to run
    // on portable CI. `run_wake_loop` is the production thread's
    // body minus the drain call, so covering it proves the
    // coalescing + shutdown semantics the `Drop` impl relies on.

    #[cfg(feature = "virtio-net-smoltcp")]
    #[test]
    fn wake_loop_runs_callback_when_signal_fires_in_isolation() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let state = Arc::new(WakeState::new());
        let counter = Arc::new(AtomicUsize::new(0));
        let state_for_thread = Arc::clone(&state);
        let c = Arc::clone(&counter);
        let handle = thread::spawn(move || {
            run_wake_loop(&state_for_thread, move || {
                c.fetch_add(1, Ordering::Relaxed);
            });
        });

        // Three fires spaced far apart — the thread should see
        // three separate drain passes.
        for _ in 0..3 {
            state.signal_rx_ready();
            // Wait until the thread has consumed this pending flag
            // before firing again. Without the busy-wait the
            // coalescing path would (correctly) merge all three
            // fires into one, which is a different test below.
            while state
                .inner
                .lock()
                .expect("test: WakeState mutex poisoned")
                .rx_pending
            {
                thread::yield_now();
            }
        }
        state.signal_shutdown();
        handle.join().unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[cfg(feature = "virtio-net-smoltcp")]
    #[test]
    fn wake_loop_coalesces_bursty_signals() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Barrier;

        // Hold the mutex to freeze the thread at `cv.wait`, fire N
        // signals, release the mutex. The thread picks up ONE
        // pending flag → runs the callback ONCE even though the
        // waker fired many times. Matches the production path
        // where an idle `socket_recv`-polling proxy fires the
        // waker a few kHz — the VMM must coalesce, not backlog.
        let state = Arc::new(WakeState::new());
        let counter = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(2));

        let state_for_thread = Arc::clone(&state);
        let c = Arc::clone(&counter);
        let barrier_for_thread = Arc::clone(&barrier);
        let handle = thread::spawn(move || {
            run_wake_loop(&state_for_thread, move || {
                c.fetch_add(1, Ordering::Relaxed);
                // Tell the main thread it's safe to signal
                // shutdown. Doing so from inside the callback
                // (WakeState mutex not held) is the race-free
                // handoff.
                barrier_for_thread.wait();
            });
        });

        // Burst: fire the waker many times while the thread is
        // parked on the condvar. All should coalesce.
        {
            let _hold = state.inner.lock().expect("test: WakeState mutex poisoned");
            // Note: while we hold the mutex, the thread can't
            // observe our writes yet. Set the flag directly here;
            // one condvar wakeup via drop(hold) is enough.
        }
        for _ in 0..100 {
            state.signal_rx_ready();
        }
        // Barrier: wait until the callback has run once (so we
        // know coalescing collapsed the 100 fires into 1 drain
        // pass). Only THEN signal shutdown.
        barrier.wait();
        state.signal_shutdown();
        handle.join().unwrap();
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "100 burst wake fires must collapse to 1 drain pass"
        );
    }

    #[cfg(feature = "virtio-net-smoltcp")]
    #[test]
    fn wake_loop_exits_on_shutdown_without_running_callback() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let state = Arc::new(WakeState::new());
        let counter = Arc::new(AtomicUsize::new(0));
        let state_for_thread = Arc::clone(&state);
        let c = Arc::clone(&counter);
        let handle = thread::spawn(move || {
            run_wake_loop(&state_for_thread, move || {
                c.fetch_add(1, Ordering::Relaxed);
            });
        });

        state.signal_shutdown();
        handle.join().unwrap();
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "Shutdown must not trigger the drain callback"
        );
    }

    #[cfg(feature = "virtio-net-smoltcp")]
    #[test]
    fn wake_loop_shutdown_independent_of_pending_rx() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Both flags set simultaneously → shutdown wins, no drain.
        // Protects against a bug where a long backlog of pending
        // RX work would block teardown behind drain passes.
        let state = Arc::new(WakeState::new());
        let counter = Arc::new(AtomicUsize::new(0));
        let state_for_thread = Arc::clone(&state);
        let c = Arc::clone(&counter);
        let handle = thread::spawn(move || {
            run_wake_loop(&state_for_thread, move || {
                c.fetch_add(1, Ordering::Relaxed);
            });
        });
        // Set both flags atomically from the main thread's side
        // before the thread observes either.
        {
            let mut inner = state.inner.lock().expect("test: WakeState mutex poisoned");
            inner.rx_pending = true;
            inner.shutdown = true;
        }
        state.cv.notify_one();
        handle.join().unwrap();
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "shutdown must win even with pending RX work queued"
        );
    }
}
