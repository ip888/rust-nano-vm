//! Split-virtqueue traversal over guest physical memory.
//!
//! The virtio-MMIO transport ([`crate::MmioTransport`]) records *where*
//! each virtqueue's three rings live in guest RAM (the descriptor table,
//! the driver/available ring, and the device/used ring) but never touches
//! guest memory itself. This module is the piece that walks those rings:
//! given a [`QueueConfig`] and something that can read/write guest physical
//! addresses ([`GuestRam`]), [`SplitQueue`] pulls buffers the guest made
//! available and hands completed buffers back via the used ring.
//!
//! It is **pure logic** — no KVM, no `unsafe`, no host-memory mapping. The
//! ring indices ([`SplitQueue`]'s `next_avail` / `next_used`) live in our
//! own address space, which is exactly what makes a running device
//! snapshot- and fork-able. `vm-kvm` supplies a [`GuestRam`] impl over its
//! mmap'd guest memory; tests supply a `Vec<u8>`-backed fake.
//!
//! # Split virtqueue layout (virtio 1.x, little-endian)
//!
//! ```text
//! Descriptor table   (QueueConfig.desc):   N * 16 bytes
//!     addr:  le64   guest-physical buffer address
//!     len:   le32   buffer length
//!     flags: le16   NEXT (1) | WRITE (2) | INDIRECT (4)
//!     next:  le16   index of the next descriptor when NEXT is set
//!
//! Available ring     (QueueConfig.driver):
//!     flags:      le16
//!     idx:        le16            driver's producer cursor
//!     ring[N]:    le16            descriptor-table head indices
//!
//! Used ring          (QueueConfig.device):
//!     flags:      le16
//!     idx:        le16            device's producer cursor
//!     ring[N]:    { id: le32, len: le32 }
//! ```
//!
//! A descriptor is **device-writable** (the device fills it — our rx path)
//! when `WRITE` is set, otherwise **device-readable** (the guest filled it —
//! our tx path). A buffer may span several descriptors chained via `NEXT`.

use std::num::Wrapping;

use thiserror::Error;

use crate::QueueConfig;

/// Descriptor flag: the buffer continues in the descriptor named by `next`.
const VIRTQ_DESC_F_NEXT: u16 = 1;
/// Descriptor flag: the buffer is device-writable (otherwise read-only).
const VIRTQ_DESC_F_WRITE: u16 = 2;
/// Descriptor flag: the buffer is itself a table of indirect descriptors.
const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/// Size of one descriptor-table entry in bytes.
const DESC_ENTRY_LEN: u64 = 16;
/// Size of one used-ring entry (`id: u32`, `len: u32`).
const USED_ENTRY_LEN: u64 = 8;
/// Largest queue size the spec permits (rings are indexed by `u16`).
const MAX_QUEUE_SIZE: u32 = 1 << 15;

/// Read/write access to guest physical memory.
///
/// Implementors must bounds-check every access against the guest's RAM and
/// return [`QueueError::OutOfBounds`] rather than panicking — a malicious or
/// buggy guest can program any address into a descriptor.
pub trait GuestRam {
    /// Fill `buf` from guest physical address `gpa`.
    fn read_at(&self, gpa: u64, buf: &mut [u8]) -> Result<(), QueueError>;
    /// Write `buf` to guest physical address `gpa`.
    fn write_at(&self, gpa: u64, buf: &[u8]) -> Result<(), QueueError>;
}

/// Errors from virtqueue traversal. None of these panic the device; the
/// consumer logs and either resets the queue or drops the offending chain.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum QueueError {
    /// A guest-physical access fell outside guest RAM.
    #[error("guest memory access out of bounds at gpa {gpa:#x} ({len} bytes)")]
    OutOfBounds {
        /// The offending guest-physical address.
        gpa: u64,
        /// The length of the attempted access.
        len: usize,
    },
    /// A descriptor index was `>= queue size`.
    #[error("descriptor index {0} out of range")]
    BadDescriptor(u16),
    /// A descriptor chain visited more descriptors than the queue holds —
    /// a cycle in the `next` links, which we refuse to follow forever.
    #[error("descriptor chain longer than queue size (cycle?)")]
    ChainTooLong,
    /// The guest set the `INDIRECT` flag; we don't advertise that feature.
    #[error("indirect descriptors are not supported")]
    IndirectUnsupported,
}

/// One resolved descriptor within a chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor {
    /// Guest-physical address of the buffer.
    pub addr: u64,
    /// Length of the buffer in bytes.
    pub len: u32,
    /// `true` if the device writes this buffer (rx), `false` if it reads it (tx).
    pub writable: bool,
}

/// A resolved descriptor chain: the head index the driver made available and
/// the ordered list of buffers it points to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescChain {
    /// Descriptor-table index of the chain head — the id echoed in the used ring.
    pub head: u16,
    /// The chain's descriptors, in `next` order.
    pub descriptors: Vec<Descriptor>,
}

impl DescChain {
    /// Total bytes across the device-readable (guest→host) descriptors.
    pub fn readable_len(&self) -> usize {
        self.descriptors
            .iter()
            .filter(|d| !d.writable)
            .map(|d| d.len as usize)
            .sum()
    }

    /// Total bytes across the device-writable (host→guest) descriptors.
    pub fn writable_len(&self) -> usize {
        self.descriptors
            .iter()
            .filter(|d| d.writable)
            .map(|d| d.len as usize)
            .sum()
    }

    /// Gather all device-readable bytes in the chain into one buffer. This
    /// is the guest→host (tx) packet the driver placed in the queue.
    pub fn read_readable(&self, mem: &impl GuestRam) -> Result<Vec<u8>, QueueError> {
        let mut out = Vec::with_capacity(self.readable_len());
        let mut scratch = Vec::new();
        for d in self.descriptors.iter().filter(|d| !d.writable) {
            let len = d.len as usize;
            scratch.resize(len, 0);
            mem.read_at(d.addr, &mut scratch)?;
            out.extend_from_slice(&scratch);
        }
        Ok(out)
    }

    /// Scatter `src` across the device-writable descriptors in order. Writes
    /// at most [`writable_len`](Self::writable_len) bytes and returns the
    /// number actually written (the lesser of `src.len()` and capacity).
    pub fn write_writable(&self, mem: &impl GuestRam, src: &[u8]) -> Result<usize, QueueError> {
        let mut off = 0usize;
        for d in self.descriptors.iter().filter(|d| d.writable) {
            if off >= src.len() {
                break;
            }
            let take = (d.len as usize).min(src.len() - off);
            mem.write_at(d.addr, &src[off..off + take])?;
            off += take;
        }
        Ok(off)
    }
}

/// A live split virtqueue: the ring addresses captured from the driver plus
/// our consumer/producer cursors. Build one from a ready [`QueueConfig`] with
/// [`SplitQueue::new`]; the cursors persist across notifications, so the
/// device holds the `SplitQueue` for the lifetime of the queue rather than
/// rebuilding it on each kick.
#[derive(Debug, Clone)]
pub struct SplitQueue {
    size: u16,
    desc: u64,
    avail: u64,
    used: u64,
    next_avail: Wrapping<u16>,
    next_used: Wrapping<u16>,
}

impl SplitQueue {
    /// Build a queue from its programmed configuration, or `None` if the
    /// queue isn't ready or the driver picked an illegal size (zero, not a
    /// power of two, or larger than the spec allows). The caller treats
    /// `None` as "skip this queue".
    pub fn new(cfg: &QueueConfig) -> Option<Self> {
        if !cfg.ready || cfg.size == 0 || cfg.size > MAX_QUEUE_SIZE || !cfg.size.is_power_of_two() {
            return None;
        }
        Some(Self {
            size: cfg.size as u16,
            desc: cfg.desc,
            avail: cfg.driver,
            used: cfg.device,
            next_avail: Wrapping(0),
            next_used: Wrapping(0),
        })
    }

    /// Number of descriptors in the ring.
    pub fn size(&self) -> u16 {
        self.size
    }

    /// Pop the next chain the driver made available, or `None` if the
    /// available ring is caught up to our consumer cursor (nothing new).
    pub fn pop_avail(&mut self, mem: &impl GuestRam) -> Result<Option<DescChain>, QueueError> {
        // avail layout: flags(2) | idx(2) | ring[size](2 each)
        let avail_idx = self.read_u16(mem, self.avail + 2)?;
        if self.next_avail.0 == avail_idx {
            return Ok(None);
        }
        let slot = u64::from(self.next_avail.0 % self.size);
        let head = self.read_u16(mem, self.avail + 4 + 2 * slot)?;
        if head >= self.size {
            return Err(QueueError::BadDescriptor(head));
        }
        let descriptors = self.collect_chain(mem, head)?;
        self.next_avail += Wrapping(1);
        Ok(Some(DescChain { head, descriptors }))
    }

    /// Mark a chain complete: write a used-ring element `{ id: head, len }`
    /// and advance the used `idx` the guest polls. `len` is the number of
    /// bytes the device wrote into the chain (0 for a consumed tx buffer).
    pub fn push_used(
        &mut self,
        mem: &impl GuestRam,
        head: u16,
        len: u32,
    ) -> Result<(), QueueError> {
        // used layout: flags(2) | idx(2) | ring[size]{ id:4, len:4 }
        let slot = u64::from(self.next_used.0 % self.size);
        let elem = self.used + 4 + USED_ENTRY_LEN * slot;
        self.write_u32(mem, elem, u32::from(head))?;
        self.write_u32(mem, elem + 4, len)?;
        self.next_used += Wrapping(1);
        // Publish the new index only after the element is fully written, so
        // the guest never observes a half-written entry.
        self.write_u16(mem, self.used + 2, self.next_used.0)?;
        Ok(())
    }

    fn collect_chain(&self, mem: &impl GuestRam, head: u16) -> Result<Vec<Descriptor>, QueueError> {
        let mut out = Vec::new();
        let mut idx = head;
        // The loop guard caps iterations at the ring size: a well-formed
        // chain can't be longer, and a cyclic `next` link can't trap us.
        for _ in 0..self.size {
            if idx >= self.size {
                return Err(QueueError::BadDescriptor(idx));
            }
            let entry = self.desc + DESC_ENTRY_LEN * u64::from(idx);
            let addr = self.read_u64(mem, entry)?;
            let len = self.read_u32(mem, entry + 8)?;
            let flags = self.read_u16(mem, entry + 12)?;
            let next = self.read_u16(mem, entry + 14)?;
            if flags & VIRTQ_DESC_F_INDIRECT != 0 {
                return Err(QueueError::IndirectUnsupported);
            }
            out.push(Descriptor {
                addr,
                len,
                writable: flags & VIRTQ_DESC_F_WRITE != 0,
            });
            if flags & VIRTQ_DESC_F_NEXT == 0 {
                return Ok(out);
            }
            idx = next;
        }
        Err(QueueError::ChainTooLong)
    }

    fn read_u16(&self, mem: &impl GuestRam, gpa: u64) -> Result<u16, QueueError> {
        let mut b = [0u8; 2];
        mem.read_at(gpa, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    fn read_u32(&self, mem: &impl GuestRam, gpa: u64) -> Result<u32, QueueError> {
        let mut b = [0u8; 4];
        mem.read_at(gpa, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn read_u64(&self, mem: &impl GuestRam, gpa: u64) -> Result<u64, QueueError> {
        let mut b = [0u8; 8];
        mem.read_at(gpa, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    fn write_u16(&self, mem: &impl GuestRam, gpa: u64, v: u16) -> Result<(), QueueError> {
        mem.write_at(gpa, &v.to_le_bytes())
    }

    fn write_u32(&self, mem: &impl GuestRam, gpa: u64, v: u32) -> Result<(), QueueError> {
        mem.write_at(gpa, &v.to_le_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Flat `Vec<u8>`-backed guest RAM starting at GPA 0. Interior mutability
    /// so reads and writes both take `&self`, matching the real mmap-backed
    /// impl in vm-kvm.
    struct FakeRam {
        mem: RefCell<Vec<u8>>,
    }

    impl FakeRam {
        fn new(size: usize) -> Self {
            Self {
                mem: RefCell::new(vec![0u8; size]),
            }
        }
        fn poke(&self, gpa: u64, bytes: &[u8]) {
            self.write_at(gpa, bytes).unwrap();
        }
        fn peek(&self, gpa: u64, len: usize) -> Vec<u8> {
            let mut v = vec![0u8; len];
            self.read_at(gpa, &mut v).unwrap();
            v
        }
    }

    impl GuestRam for FakeRam {
        fn read_at(&self, gpa: u64, buf: &mut [u8]) -> Result<(), QueueError> {
            let mem = self.mem.borrow();
            let start = gpa as usize;
            let end = start
                .checked_add(buf.len())
                .filter(|&e| e <= mem.len())
                .ok_or(QueueError::OutOfBounds {
                    gpa,
                    len: buf.len(),
                })?;
            buf.copy_from_slice(&mem[start..end]);
            Ok(())
        }
        fn write_at(&self, gpa: u64, buf: &[u8]) -> Result<(), QueueError> {
            let mut mem = self.mem.borrow_mut();
            let start = gpa as usize;
            let end = start
                .checked_add(buf.len())
                .filter(|&e| e <= mem.len())
                .ok_or(QueueError::OutOfBounds {
                    gpa,
                    len: buf.len(),
                })?;
            mem[start..end].copy_from_slice(buf);
            Ok(())
        }
    }

    // Ring layout used by most tests. Queue size 4.
    //   desc   @ 0x1000  (4 * 16 = 64 bytes)
    //   avail  @ 0x1100
    //   used   @ 0x1200
    //   buffers@ 0x2000+
    const SIZE: u32 = 4;
    const DESC: u64 = 0x1000;
    const AVAIL: u64 = 0x1100;
    const USED: u64 = 0x1200;

    fn cfg() -> QueueConfig {
        QueueConfig {
            size: SIZE,
            ready: true,
            desc: DESC,
            driver: AVAIL,
            device: USED,
        }
    }

    /// Write descriptor `idx`: addr/len/flags/next.
    fn write_desc(ram: &FakeRam, idx: u64, addr: u64, len: u32, flags: u16, next: u16) {
        let base = DESC + 16 * idx;
        ram.poke(base, &addr.to_le_bytes());
        ram.poke(base + 8, &len.to_le_bytes());
        ram.poke(base + 12, &flags.to_le_bytes());
        ram.poke(base + 14, &next.to_le_bytes());
    }

    /// Set avail.ring[slot] = head and avail.idx = idx.
    fn publish_avail(ram: &FakeRam, slot: u64, head: u16, idx: u16) {
        ram.poke(AVAIL + 4 + 2 * slot, &head.to_le_bytes());
        ram.poke(AVAIL + 2, &idx.to_le_bytes());
    }

    #[test]
    fn new_rejects_unready_or_bad_sizes() {
        let mut c = cfg();
        c.ready = false;
        assert!(SplitQueue::new(&c).is_none());

        let mut c = cfg();
        c.size = 0;
        assert!(SplitQueue::new(&c).is_none());

        let mut c = cfg();
        c.size = 6; // not a power of two
        assert!(SplitQueue::new(&c).is_none());

        let mut c = cfg();
        c.size = MAX_QUEUE_SIZE * 2;
        assert!(SplitQueue::new(&c).is_none());

        assert!(SplitQueue::new(&cfg()).is_some());
    }

    #[test]
    fn pop_returns_none_when_no_avail() {
        let ram = FakeRam::new(0x4000);
        let mut q = SplitQueue::new(&cfg()).unwrap();
        // avail.idx == 0 == next_avail → nothing available.
        assert_eq!(q.pop_avail(&ram).unwrap(), None);
    }

    #[test]
    fn pop_single_readable_descriptor_and_read_bytes() {
        let ram = FakeRam::new(0x4000);
        let payload = b"hello vsock";
        ram.poke(0x2000, payload);
        write_desc(&ram, 0, 0x2000, payload.len() as u32, 0, 0);
        publish_avail(&ram, 0, 0, 1);

        let mut q = SplitQueue::new(&cfg()).unwrap();
        let chain = q.pop_avail(&ram).unwrap().expect("a chain");
        assert_eq!(chain.head, 0);
        assert_eq!(chain.descriptors.len(), 1);
        assert!(!chain.descriptors[0].writable);
        assert_eq!(chain.readable_len(), payload.len());
        assert_eq!(chain.read_readable(&ram).unwrap(), payload);

        // Consumer cursor advanced: a second pop sees nothing.
        assert_eq!(q.pop_avail(&ram).unwrap(), None);
    }

    #[test]
    fn pop_multi_descriptor_chain_concatenates_readable_bytes() {
        let ram = FakeRam::new(0x4000);
        ram.poke(0x2000, b"foo");
        ram.poke(0x2100, b"bar");
        // desc 0 -> desc 1 (NEXT), both readable.
        write_desc(&ram, 0, 0x2000, 3, VIRTQ_DESC_F_NEXT, 1);
        write_desc(&ram, 1, 0x2100, 3, 0, 0);
        publish_avail(&ram, 0, 0, 1);

        let mut q = SplitQueue::new(&cfg()).unwrap();
        let chain = q.pop_avail(&ram).unwrap().unwrap();
        assert_eq!(chain.descriptors.len(), 2);
        assert_eq!(chain.read_readable(&ram).unwrap(), b"foobar");
    }

    #[test]
    fn write_writable_scatters_across_chain() {
        let ram = FakeRam::new(0x4000);
        // Two writable descriptors of 4 bytes each at distinct buffers.
        write_desc(
            &ram,
            0,
            0x2000,
            4,
            VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT,
            1,
        );
        write_desc(&ram, 1, 0x3000, 4, VIRTQ_DESC_F_WRITE, 0);
        publish_avail(&ram, 0, 0, 1);

        let mut q = SplitQueue::new(&cfg()).unwrap();
        let chain = q.pop_avail(&ram).unwrap().unwrap();
        assert_eq!(chain.writable_len(), 8);

        let written = chain.write_writable(&ram, b"ABCDEFGH").unwrap();
        assert_eq!(written, 8);
        assert_eq!(ram.peek(0x2000, 4), b"ABCD");
        assert_eq!(ram.peek(0x3000, 4), b"EFGH");
    }

    #[test]
    fn write_writable_truncates_to_capacity() {
        let ram = FakeRam::new(0x4000);
        write_desc(&ram, 0, 0x2000, 4, VIRTQ_DESC_F_WRITE, 0);
        publish_avail(&ram, 0, 0, 1);
        let mut q = SplitQueue::new(&cfg()).unwrap();
        let chain = q.pop_avail(&ram).unwrap().unwrap();
        // Source larger than the single 4-byte buffer.
        let written = chain.write_writable(&ram, b"0123456789").unwrap();
        assert_eq!(written, 4);
        assert_eq!(ram.peek(0x2000, 4), b"0123");
    }

    #[test]
    fn push_used_writes_element_and_advances_idx() {
        let ram = FakeRam::new(0x4000);
        let mut q = SplitQueue::new(&cfg()).unwrap();
        q.push_used(&ram, 2, 11).unwrap();

        // used.ring[0] = { id: 2, len: 11 }
        assert_eq!(
            u32::from_le_bytes(ram.peek(USED + 4, 4).try_into().unwrap()),
            2
        );
        assert_eq!(
            u32::from_le_bytes(ram.peek(USED + 8, 4).try_into().unwrap()),
            11
        );
        // used.idx advanced to 1.
        assert_eq!(
            u16::from_le_bytes(ram.peek(USED + 2, 2).try_into().unwrap()),
            1
        );
    }

    #[test]
    fn cursors_wrap_around_the_ring() {
        let ram = FakeRam::new(0x4000);
        ram.poke(0x2000, b"x");
        write_desc(&ram, 0, 0x2000, 1, 0, 0);

        let mut q = SplitQueue::new(&cfg()).unwrap();
        // Drive more iterations than the ring is wide (size 4) to force the
        // modulo wrap on both avail and used cursors.
        for i in 0..10u16 {
            publish_avail(&ram, u64::from(i % SIZE as u16), 0, i + 1);
            let chain = q.pop_avail(&ram).unwrap().expect("chain each round");
            assert_eq!(chain.read_readable(&ram).unwrap(), b"x");
            q.push_used(&ram, chain.head, 1).unwrap();
        }
        // After 10 completions, used.idx wrapped to 10.
        assert_eq!(
            u16::from_le_bytes(ram.peek(USED + 2, 2).try_into().unwrap()),
            10
        );
    }

    #[test]
    fn bad_head_index_is_rejected() {
        let ram = FakeRam::new(0x4000);
        publish_avail(&ram, 0, 99, 1); // head 99 >= size 4
        let mut q = SplitQueue::new(&cfg()).unwrap();
        assert_eq!(q.pop_avail(&ram), Err(QueueError::BadDescriptor(99)));
    }

    #[test]
    fn cyclic_chain_is_rejected_as_too_long() {
        let ram = FakeRam::new(0x4000);
        // desc 0 -> 1 -> 0 -> ... cycle, all NEXT.
        write_desc(&ram, 0, 0x2000, 1, VIRTQ_DESC_F_NEXT, 1);
        write_desc(&ram, 1, 0x2000, 1, VIRTQ_DESC_F_NEXT, 0);
        publish_avail(&ram, 0, 0, 1);
        let mut q = SplitQueue::new(&cfg()).unwrap();
        assert_eq!(q.pop_avail(&ram), Err(QueueError::ChainTooLong));
    }

    #[test]
    fn indirect_descriptors_are_rejected() {
        let ram = FakeRam::new(0x4000);
        write_desc(&ram, 0, 0x2000, 16, VIRTQ_DESC_F_INDIRECT, 0);
        publish_avail(&ram, 0, 0, 1);
        let mut q = SplitQueue::new(&cfg()).unwrap();
        assert_eq!(q.pop_avail(&ram), Err(QueueError::IndirectUnsupported));
    }

    #[test]
    fn out_of_bounds_buffer_address_surfaces_as_error() {
        let ram = FakeRam::new(0x4000);
        // Buffer past the end of RAM.
        write_desc(&ram, 0, 0x9000, 8, 0, 0);
        publish_avail(&ram, 0, 0, 1);
        let mut q = SplitQueue::new(&cfg()).unwrap();
        let chain = q.pop_avail(&ram).unwrap().unwrap();
        assert!(matches!(
            chain.read_readable(&ram),
            Err(QueueError::OutOfBounds { .. })
        ));
    }
}
