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

use virtio_queue::{GuestMemory, QueueError};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

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
// `dead_code` is a temporary artifact of sub-PR #C's commit sequence:
// this adapter is used by the queue consumer wiring that lands two
// commits from now (MMIO exit routing). Granting the exception here
// instead of up at the module level narrows the escape hatch to one
// item and makes the next commit a mechanical removal.
#[allow(dead_code)]
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
