//! Split virtqueue primitives.
//!
//! Scope: what every virtio device (vsock, fs, net, ...) needs in common —
//! the descriptor table entry, flag constants, the cycle-safe descriptor
//! chain iterator, and the available / used ring views over caller-owned
//! byte buffers. Device-specific logic (vsock packet framing, virtio-fs
//! FUSE framing, ...) lives in each device crate.
//!
//! Deferred to follow-up PRs: indirect descriptors, packed (vs split)
//! virtqueue, guest-memory integration (`vm-memory`), KVM eventfd
//! plumbing.
//!
//! # Wire format
//!
//! Descriptor table entry (16 bytes, all little-endian, virtio 1.3 §2.7):
//!
//! ```c
//! struct virtq_desc {
//!     __le64 addr;    // guest-physical address of buffer
//!     __le32 len;     // buffer length
//!     __le16 flags;   // DESC_F_NEXT | DESC_F_WRITE | DESC_F_INDIRECT
//!     __le16 next;    // index of next descriptor in chain, if F_NEXT set
//! };
//! ```
//!
//! Available ring (driver → device, `6 + 2 * qsize` bytes):
//!
//! ```c
//! struct virtq_avail {
//!     __le16 flags;             // VIRTQ_AVAIL_F_NO_INTERRUPT
//!     __le16 idx;               // monotonic write counter, wraps at u16
//!     __le16 ring[qsize];       // descriptor head indices
//!     __le16 used_event;        // VIRTQ_F_EVENT_IDX feature
//! };
//! ```
//!
//! Used ring (device → driver, `6 + 8 * qsize` bytes):
//!
//! ```c
//! struct virtq_used_elem {
//!     __le32 id;                // start of used descriptor chain (head)
//!     __le32 len;               // bytes written into device-writable area
//! };
//! struct virtq_used {
//!     __le16 flags;             // VIRTQ_USED_F_NO_NOTIFY
//!     __le16 idx;               // monotonic write counter, wraps at u16
//!     struct virtq_used_elem ring[qsize];
//!     __le16 avail_event;       // VIRTQ_F_EVENT_IDX feature
//! };
//! ```
//!
//! All offsets are pinned by virtio 1.3 §2.7 and verified by tests.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use thiserror::Error;

/// On-the-wire size of a single descriptor in bytes.
pub const DESC_SIZE: usize = 16;

/// "Buffer continues in the descriptor at `next`."
pub const DESC_F_NEXT: u16 = 1 << 0;

/// "Buffer is device-writable" (i.e. a guest-provided rx slot). Without
/// this flag set the buffer is device-readable (tx).
pub const DESC_F_WRITE: u16 = 1 << 1;

/// "`addr` points to a table of further descriptors rather than a buffer."
/// Not walked by [`DescriptorChain`] today — follow-up PR.
pub const DESC_F_INDIRECT: u16 = 1 << 2;

/// A single entry in the descriptor table.
///
/// `addr` is a guest-physical address; this crate does not dereference it.
/// Resolving the buffer lives one layer up, on top of a `vm-memory`-style
/// `GuestMemory` trait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor {
    /// Guest-physical address of the buffer this descriptor points at.
    pub addr: u64,
    /// Length of the buffer in bytes.
    pub len: u32,
    /// Bitfield of `DESC_F_*` flags.
    pub flags: u16,
    /// Index of the next descriptor in the chain, valid iff
    /// `flags & DESC_F_NEXT != 0`.
    pub next: u16,
}

impl Descriptor {
    /// Parse a little-endian descriptor from the first [`DESC_SIZE`] bytes
    /// of `buf`. `buf` must be **at least** that long; trailing bytes are
    /// ignored (common when the caller scans a descriptor table slice by
    /// advancing 16 bytes at a time).
    pub fn from_bytes(buf: &[u8]) -> Result<Self, QueueError> {
        if buf.len() < DESC_SIZE {
            return Err(QueueError::ShortDescriptor {
                have: buf.len(),
                need: DESC_SIZE,
            });
        }
        Ok(Self {
            addr: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            len: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            flags: u16::from_le_bytes(buf[12..14].try_into().unwrap()),
            next: u16::from_le_bytes(buf[14..16].try_into().unwrap()),
        })
    }

    /// Serialize to `buf`, which must be at least [`DESC_SIZE`] bytes.
    /// Writes exactly [`DESC_SIZE`] bytes and returns that count.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, QueueError> {
        if buf.len() < DESC_SIZE {
            return Err(QueueError::ShortBuffer {
                have: buf.len(),
                need: DESC_SIZE,
            });
        }
        buf[0..8].copy_from_slice(&self.addr.to_le_bytes());
        buf[8..12].copy_from_slice(&self.len.to_le_bytes());
        buf[12..14].copy_from_slice(&self.flags.to_le_bytes());
        buf[14..16].copy_from_slice(&self.next.to_le_bytes());
        Ok(DESC_SIZE)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; DESC_SIZE] {
        let mut out = [0u8; DESC_SIZE];
        // Cannot fail: we just allocated exactly DESC_SIZE bytes. The
        // expect() ensures any future validation added to write_to trips
        // loudly instead of being silently dropped.
        self.write_to(&mut out)
            .expect("serializing Descriptor into a fixed-size buffer must succeed");
        out
    }

    /// `true` when the chain continues via [`Descriptor::next`].
    pub fn has_next(&self) -> bool {
        self.flags & DESC_F_NEXT != 0
    }

    /// `true` when this buffer is device-writable (i.e. a guest-provided rx
    /// slot). Otherwise it's device-readable (tx from guest to device).
    pub fn is_writable(&self) -> bool {
        self.flags & DESC_F_WRITE != 0
    }

    /// `true` when `addr` points at an indirect descriptor table rather
    /// than a buffer. Callers must walk the indirect table separately;
    /// [`DescriptorChain`] does not follow indirect links today.
    pub fn is_indirect(&self) -> bool {
        self.flags & DESC_F_INDIRECT != 0
    }
}

/// Iterator that walks a descriptor chain starting at a given head index.
///
/// The walker is cycle-safe: it stops after visiting `table.len()`
/// descriptors, which is the maximum legal chain length per the virtio
/// spec. A malicious guest that crafts a self-referencing chain gets an
/// early termination via [`QueueError::ChainTooLong`] rather than an
/// infinite loop.
///
/// It also validates each `next` index against the table bounds and
/// reports [`QueueError::BadIndex`] if a chain descriptor points outside
/// the table.
///
/// Errors are surfaced by making [`Iterator::Item`] a `Result`, not by
/// panicking or silently truncating.
pub struct DescriptorChain<'a> {
    table: &'a [Descriptor],
    next: Option<u16>,
    visited: usize,
    failed: bool,
}

impl<'a> DescriptorChain<'a> {
    /// Start a new chain at descriptor `head` within `table`.
    pub fn new(table: &'a [Descriptor], head: u16) -> Self {
        Self {
            table,
            next: Some(head),
            visited: 0,
            failed: false,
        }
    }
}

impl<'a> Iterator for DescriptorChain<'a> {
    type Item = Result<&'a Descriptor, QueueError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let idx = self.next?;
        let idx_usize = idx as usize;
        let Some(desc) = self.table.get(idx_usize) else {
            self.failed = true;
            return Some(Err(QueueError::BadIndex {
                idx,
                table_len: self.table.len(),
            }));
        };
        self.visited += 1;
        if self.visited > self.table.len() {
            self.failed = true;
            return Some(Err(QueueError::ChainTooLong {
                limit: self.table.len(),
            }));
        }
        self.next = if desc.has_next() {
            Some(desc.next)
        } else {
            None
        };
        Some(Ok(desc))
    }
}

impl<'a> std::fmt::Debug for DescriptorChain<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DescriptorChain")
            .field("table_len", &self.table.len())
            .field("next", &self.next)
            .field("visited", &self.visited)
            .field("failed", &self.failed)
            .finish()
    }
}

/// Errors produced by virtqueue parsing.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum QueueError {
    /// Byte slice presented for descriptor parsing is smaller than
    /// [`DESC_SIZE`].
    #[error("descriptor too short: have {have} bytes, need {need}")]
    ShortDescriptor {
        /// Bytes we were handed.
        have: usize,
        /// Bytes the descriptor requires.
        need: usize,
    },
    /// Output buffer presented for serialization is smaller than
    /// [`DESC_SIZE`].
    #[error("descriptor output buffer too small: have {have} bytes, need {need}")]
    ShortBuffer {
        /// Bytes in the output buffer.
        have: usize,
        /// Bytes the serialized descriptor requires.
        need: usize,
    },
    /// A chain walk reached an index outside the descriptor table.
    #[error("descriptor chain index {idx} out of bounds for table of length {table_len}")]
    BadIndex {
        /// Offending index.
        idx: u16,
        /// Length of the descriptor table.
        table_len: usize,
    },
    /// A chain walk visited more descriptors than the table contains —
    /// almost certainly a cycle or a malicious crafted chain.
    #[error("descriptor chain longer than table size {limit}; possible cycle")]
    ChainTooLong {
        /// Max chain length we permit (equal to table size).
        limit: usize,
    },
    /// Backing buffer for an available / used ring is smaller than the
    /// ring's nominal size for the given queue size.
    #[error("ring buffer too small: have {have} bytes, need {need} (qsize={qsize})")]
    ShortRing {
        /// Bytes in the buffer we were handed.
        have: usize,
        /// Bytes the ring requires for the configured queue size.
        need: usize,
        /// Configured queue size.
        qsize: u16,
    },
    /// Queue size is zero, larger than [`MAX_QUEUE_SIZE`], or not a power
    /// of two — virtio requires the latter.
    #[error("invalid queue size {qsize}; must be a power of two in 1..={max}")]
    BadQueueSize {
        /// Queue size we were handed.
        qsize: u16,
        /// Maximum allowed queue size.
        max: u16,
    },
}

// ---------------------------------------------------------------------------
// Ring constants
// ---------------------------------------------------------------------------

/// Maximum queue size per the virtio spec: 2^15.
pub const MAX_QUEUE_SIZE: u16 = 32_768;

/// Set in [`AvailRing::flags`] when the driver doesn't want the device to
/// send an interrupt for completed buffers.
pub const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1 << 0;

/// Set in [`UsedRing::flags`] when the device doesn't want the driver to
/// kick (notify) for newly-supplied descriptors.
pub const VIRTQ_USED_F_NO_NOTIFY: u16 = 1 << 0;

/// Bytes occupied by an available ring of the given queue size:
/// `flags(2) + idx(2) + ring(2 * qsize) + used_event(2)`.
pub fn avail_ring_size(qsize: u16) -> usize {
    6 + 2 * qsize as usize
}

/// Bytes occupied by a used ring of the given queue size:
/// `flags(2) + idx(2) + ring(8 * qsize) + avail_event(2)`.
pub fn used_ring_size(qsize: u16) -> usize {
    6 + 8 * qsize as usize
}

fn validate_qsize(qsize: u16) -> Result<(), QueueError> {
    if qsize == 0 || qsize > MAX_QUEUE_SIZE || !qsize.is_power_of_two() {
        return Err(QueueError::BadQueueSize {
            qsize,
            max: MAX_QUEUE_SIZE,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Available ring (driver → device, read-only on our side)
// ---------------------------------------------------------------------------

/// Borrowed read-only view of a virtio split available ring.
///
/// The driver writes; the device (us) reads. `bytes` must be backed by a
/// `2 * qsize + 6` byte slab inside guest memory. Multi-byte fields are
/// interpreted little-endian by the accessors.
///
/// All slot reads use modular arithmetic against `qsize`, matching the
/// driver's wraparound semantics — so a producer that has wrapped its
/// `idx` field many times still indexes valid slots.
#[derive(Debug, Clone, Copy)]
pub struct AvailRing<'a> {
    bytes: &'a [u8],
    qsize: u16,
}

impl<'a> AvailRing<'a> {
    /// Wrap `bytes` as an available ring of size `qsize`. Validates that
    /// `qsize` is a power of two in `1..=MAX_QUEUE_SIZE` and that `bytes`
    /// is at least [`avail_ring_size`] bytes.
    pub fn new(bytes: &'a [u8], qsize: u16) -> Result<Self, QueueError> {
        validate_qsize(qsize)?;
        let need = avail_ring_size(qsize);
        if bytes.len() < need {
            return Err(QueueError::ShortRing {
                have: bytes.len(),
                need,
                qsize,
            });
        }
        Ok(Self { bytes, qsize })
    }

    /// Configured queue size.
    pub fn qsize(&self) -> u16 {
        self.qsize
    }

    /// `flags` field (offset 0).
    pub fn flags(&self) -> u16 {
        u16::from_le_bytes(self.bytes[0..2].try_into().unwrap())
    }

    /// Driver's monotonic-mod-2^16 producer index (offset 2). The number of
    /// available descriptor heads is `idx().wrapping_sub(last_seen)`; slots
    /// are read mod `qsize`.
    pub fn idx(&self) -> u16 {
        u16::from_le_bytes(self.bytes[2..4].try_into().unwrap())
    }

    /// Descriptor head index at `slot` (which is implicitly taken mod
    /// `qsize`).
    pub fn head(&self, slot: u16) -> u16 {
        let i = (slot % self.qsize) as usize;
        let off = 4 + 2 * i;
        u16::from_le_bytes(self.bytes[off..off + 2].try_into().unwrap())
    }

    /// `used_event` field at the very end of the ring. Only meaningful
    /// when the `VIRTQ_F_EVENT_IDX` feature is negotiated; otherwise the
    /// bytes are reserved.
    pub fn used_event(&self) -> u16 {
        let off = 4 + 2 * self.qsize as usize;
        u16::from_le_bytes(self.bytes[off..off + 2].try_into().unwrap())
    }

    /// Iterate descriptor heads added by the driver since `last_seen`
    /// (the consumer's previous read of `idx()`). Yields heads in the
    /// order the driver produced them.
    pub fn iter_new(&self, last_seen: u16) -> AvailIter<'_, 'a> {
        AvailIter {
            ring: self,
            next: last_seen,
            end: self.idx(),
        }
    }
}

/// Iterator yielded by [`AvailRing::iter_new`].
#[derive(Debug)]
pub struct AvailIter<'r, 'a> {
    ring: &'r AvailRing<'a>,
    next: u16,
    end: u16,
}

impl<'r, 'a> Iterator for AvailIter<'r, 'a> {
    type Item = u16;
    fn next(&mut self) -> Option<u16> {
        if self.next == self.end {
            return None;
        }
        let head = self.ring.head(self.next);
        self.next = self.next.wrapping_add(1);
        Some(head)
    }
}

// ---------------------------------------------------------------------------
// Used ring (device → driver, read+write on our side)
// ---------------------------------------------------------------------------

/// Borrowed mutable view of a virtio split used ring.
///
/// The device (us) writes; the driver reads. Each completed descriptor
/// chain is reported via [`UsedRing::push`]: write `(head_idx, written_len)`
/// into `ring[idx % qsize]`, then advance `idx`.
#[derive(Debug)]
pub struct UsedRing<'a> {
    bytes: &'a mut [u8],
    qsize: u16,
}

impl<'a> UsedRing<'a> {
    /// Wrap `bytes` as a used ring of size `qsize`. Validates `qsize` and
    /// buffer length the same way as [`AvailRing::new`].
    pub fn new(bytes: &'a mut [u8], qsize: u16) -> Result<Self, QueueError> {
        validate_qsize(qsize)?;
        let need = used_ring_size(qsize);
        if bytes.len() < need {
            return Err(QueueError::ShortRing {
                have: bytes.len(),
                need,
                qsize,
            });
        }
        Ok(Self { bytes, qsize })
    }

    /// Configured queue size.
    pub fn qsize(&self) -> u16 {
        self.qsize
    }

    /// `flags` field (offset 0).
    pub fn flags(&self) -> u16 {
        u16::from_le_bytes(self.bytes[0..2].try_into().unwrap())
    }

    /// Set the `flags` field.
    pub fn set_flags(&mut self, v: u16) {
        self.bytes[0..2].copy_from_slice(&v.to_le_bytes());
    }

    /// Device's monotonic-mod-2^16 producer index (offset 2).
    pub fn idx(&self) -> u16 {
        u16::from_le_bytes(self.bytes[2..4].try_into().unwrap())
    }

    /// Set `idx`. Real callers should prefer [`Self::push`], which wraps
    /// the slot/index dance.
    pub fn set_idx(&mut self, v: u16) {
        self.bytes[2..4].copy_from_slice(&v.to_le_bytes());
    }

    /// `avail_event` field at the very end of the ring. Only meaningful
    /// when `VIRTQ_F_EVENT_IDX` is negotiated.
    pub fn avail_event(&self) -> u16 {
        let off = 4 + 8 * self.qsize as usize;
        u16::from_le_bytes(self.bytes[off..off + 2].try_into().unwrap())
    }

    /// Set `avail_event`.
    pub fn set_avail_event(&mut self, v: u16) {
        let off = 4 + 8 * self.qsize as usize;
        self.bytes[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }

    /// Read used-elem at the given slot (taken mod `qsize`). Returns
    /// `(head_idx, written_len)`.
    pub fn elem(&self, slot: u16) -> (u32, u32) {
        let i = (slot % self.qsize) as usize;
        let off = 4 + 8 * i;
        let id = u32::from_le_bytes(self.bytes[off..off + 4].try_into().unwrap());
        let len = u32::from_le_bytes(self.bytes[off + 4..off + 8].try_into().unwrap());
        (id, len)
    }

    /// Append a used-elem at the slot indicated by current `idx() %
    /// qsize` and advance `idx` by one (with wraparound at `u16::MAX`).
    /// Returns the slot index that was written.
    ///
    /// `head_idx` is the descriptor table index that started the chain
    /// the driver gave us; `written_len` is the number of bytes the
    /// device wrote into the device-writable portion of that chain.
    pub fn push(&mut self, head_idx: u32, written_len: u32) -> u16 {
        let cur = self.idx();
        let slot = (cur % self.qsize) as usize;
        let off = 4 + 8 * slot;
        self.bytes[off..off + 4].copy_from_slice(&head_idx.to_le_bytes());
        self.bytes[off + 4..off + 8].copy_from_slice(&written_len.to_le_bytes());
        let new_idx = cur.wrapping_add(1);
        self.set_idx(new_idx);
        cur
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desc_size_is_exactly_16() {
        assert_eq!(DESC_SIZE, 16);
    }

    #[test]
    fn roundtrip_preserves_every_field() {
        let d = Descriptor {
            addr: 0xdead_beef_cafe_f00d,
            len: 4096,
            flags: DESC_F_NEXT | DESC_F_WRITE,
            next: 7,
        };
        let bytes = d.to_bytes();
        assert_eq!(bytes.len(), DESC_SIZE);
        let decoded = Descriptor::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, d);
    }

    #[test]
    fn field_offsets_match_virtio_spec() {
        let d = Descriptor {
            addr: 0x0101_0101_0101_0101,
            len: 0x0202_0202,
            flags: 0x0303,
            next: 0x0404,
        };
        let b = d.to_bytes();
        assert_eq!(&b[0..8], &[0x01; 8]);
        assert_eq!(&b[8..12], &[0x02; 4]);
        assert_eq!(&b[12..14], &[0x03, 0x03]);
        assert_eq!(&b[14..16], &[0x04, 0x04]);
    }

    #[test]
    fn flag_accessors_decode_each_bit() {
        let mut d = Descriptor {
            addr: 0,
            len: 0,
            flags: 0,
            next: 0,
        };
        assert!(!d.has_next() && !d.is_writable() && !d.is_indirect());

        d.flags = DESC_F_NEXT;
        assert!(d.has_next() && !d.is_writable() && !d.is_indirect());

        d.flags = DESC_F_WRITE;
        assert!(!d.has_next() && d.is_writable() && !d.is_indirect());

        d.flags = DESC_F_INDIRECT;
        assert!(!d.has_next() && !d.is_writable() && d.is_indirect());

        d.flags = DESC_F_NEXT | DESC_F_WRITE | DESC_F_INDIRECT;
        assert!(d.has_next() && d.is_writable() && d.is_indirect());
    }

    #[test]
    fn from_bytes_rejects_short_input() {
        let short = [0u8; DESC_SIZE - 1];
        let err = Descriptor::from_bytes(&short).unwrap_err();
        assert_eq!(
            err,
            QueueError::ShortDescriptor {
                have: DESC_SIZE - 1,
                need: DESC_SIZE,
            }
        );
    }

    #[test]
    fn from_bytes_accepts_longer_buffer_and_ignores_trailing_bytes() {
        // Scanning a raw descriptor table slice is the common case; the
        // caller shouldn't have to exactly-slice to DESC_SIZE.
        let d = Descriptor {
            addr: 0xdead_beef,
            len: 64,
            flags: DESC_F_NEXT,
            next: 3,
        };
        let mut buf = Vec::with_capacity(DESC_SIZE + 16);
        buf.extend_from_slice(&d.to_bytes());
        buf.extend_from_slice(&[0xAB; 16]);
        let decoded = Descriptor::from_bytes(&buf).expect("longer buffer must parse");
        assert_eq!(decoded, d);
    }

    #[test]
    fn write_to_rejects_short_output() {
        let d = Descriptor {
            addr: 0,
            len: 0,
            flags: 0,
            next: 0,
        };
        let mut buf = [0u8; 8];
        let err = d.write_to(&mut buf).unwrap_err();
        assert_eq!(
            err,
            QueueError::ShortBuffer {
                have: 8,
                need: DESC_SIZE,
            }
        );
    }

    fn desc(addr: u64, len: u32, flags: u16, next: u16) -> Descriptor {
        Descriptor {
            addr,
            len,
            flags,
            next,
        }
    }

    #[test]
    fn chain_with_single_descriptor_yields_one_item() {
        // Single descriptor, no NEXT flag.
        let table = [desc(0x1000, 4096, 0, 0)];
        let chain: Result<Vec<_>, _> = DescriptorChain::new(&table, 0).collect();
        let descs = chain.unwrap();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].addr, 0x1000);
    }

    #[test]
    fn chain_walks_linked_list_following_next() {
        // head=0 → next=2 → next=1 (tail). Descriptors are stored out of
        // order in the table on purpose; a chain is a linked list, not an
        // array slice.
        let table = [
            desc(0x11, 100, DESC_F_NEXT, 2), // idx 0 -> 2
            desc(0x22, 200, 0, 0),           // idx 1 tail
            desc(0x33, 300, DESC_F_NEXT, 1), // idx 2 -> 1
        ];
        let chain: Result<Vec<_>, _> = DescriptorChain::new(&table, 0).collect();
        let descs = chain.unwrap();
        assert_eq!(descs.len(), 3);
        assert_eq!(descs[0].addr, 0x11);
        assert_eq!(descs[1].addr, 0x33);
        assert_eq!(descs[2].addr, 0x22);
    }

    #[test]
    fn chain_surfaces_bad_index_when_next_points_outside_table() {
        // head=0 has NEXT set but `next` points past the end of the table.
        let table = [desc(0x11, 100, DESC_F_NEXT, 99)];
        let results: Vec<_> = DescriptorChain::new(&table, 0).collect();
        // First yield is the descriptor, second is the error.
        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        assert!(matches!(
            results[1],
            Err(QueueError::BadIndex {
                idx: 99,
                table_len: 1
            })
        ));
    }

    #[test]
    fn chain_surfaces_bad_index_when_head_is_out_of_range() {
        let table = [desc(0x11, 100, 0, 0)];
        let results: Vec<_> = DescriptorChain::new(&table, 42).collect();
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            Err(QueueError::BadIndex {
                idx: 42,
                table_len: 1
            })
        ));
    }

    #[test]
    fn chain_rejects_cycle_before_infinite_loop() {
        // 0 -> 1 -> 0 -> 1 -> ... cycle.
        let table = [
            desc(0x11, 10, DESC_F_NEXT, 1),
            desc(0x22, 20, DESC_F_NEXT, 0),
        ];
        let results: Vec<_> = DescriptorChain::new(&table, 0).collect();
        // We visit table.len() descriptors ok, then detect the next would
        // exceed the cap.
        let last = results.last().unwrap();
        assert!(matches!(last, Err(QueueError::ChainTooLong { limit: 2 })));
    }

    #[test]
    fn chain_stops_yielding_after_an_error() {
        // After a BadIndex error the iterator should be fused.
        let table = [desc(0x11, 100, DESC_F_NEXT, 99)];
        let mut chain = DescriptorChain::new(&table, 0);
        assert!(chain.next().unwrap().is_ok()); // descriptor 0
        assert!(chain.next().unwrap().is_err()); // BadIndex
        assert!(chain.next().is_none()); // fused
        assert!(chain.next().is_none());
    }

    // ---- Ring sizing + qsize validation -------------------------------

    #[test]
    fn ring_sizes_match_spec_formulas() {
        // 6 + 2*qsize for avail, 6 + 8*qsize for used.
        assert_eq!(avail_ring_size(8), 22);
        assert_eq!(used_ring_size(8), 70);
        assert_eq!(avail_ring_size(256), 518);
        assert_eq!(used_ring_size(256), 2054);
    }

    #[test]
    fn validate_qsize_accepts_powers_of_two_in_range() {
        for qs in [1u16, 2, 4, 8, 16, 256, 1024, MAX_QUEUE_SIZE] {
            let mut buf = vec![0u8; avail_ring_size(qs)];
            buf.extend(std::iter::once(0)); // pad
            assert!(AvailRing::new(&buf, qs).is_ok(), "qsize={qs} rejected");
        }
    }

    #[test]
    fn validate_qsize_rejects_zero_non_power_of_two_and_out_of_range() {
        let buf = [0u8; 1024];
        for qs in [0u16, 3, 5, 7, 100, 1000] {
            let err = AvailRing::new(&buf, qs).unwrap_err();
            assert!(matches!(err, QueueError::BadQueueSize { .. }), "qs={qs}");
        }
    }

    #[test]
    fn ring_constructors_reject_short_buffers() {
        let buf = [0u8; 4];
        let err = AvailRing::new(&buf, 8).unwrap_err();
        assert!(matches!(
            err,
            QueueError::ShortRing {
                have: 4,
                need: 22,
                qsize: 8
            }
        ));
        let mut buf = [0u8; 4];
        let err = UsedRing::new(&mut buf, 8).unwrap_err();
        assert!(matches!(
            err,
            QueueError::ShortRing {
                have: 4,
                need: 70,
                qsize: 8
            }
        ));
    }

    // ---- AvailRing -----------------------------------------------------

    /// Build an avail-ring backing buffer with the given heads and idx.
    fn avail_buf(qsize: u16, idx: u16, heads: &[u16], used_event: u16) -> Vec<u8> {
        let mut buf = vec![0u8; avail_ring_size(qsize)];
        // flags = 0
        buf[2..4].copy_from_slice(&idx.to_le_bytes());
        for (i, h) in heads.iter().enumerate() {
            let off = 4 + 2 * i;
            buf[off..off + 2].copy_from_slice(&h.to_le_bytes());
        }
        let off = 4 + 2 * qsize as usize;
        buf[off..off + 2].copy_from_slice(&used_event.to_le_bytes());
        buf
    }

    #[test]
    fn avail_ring_reads_flags_idx_and_heads() {
        let buf = avail_buf(4, 3, &[10, 20, 30, 0], 7);
        let ring = AvailRing::new(&buf, 4).unwrap();
        assert_eq!(ring.qsize(), 4);
        assert_eq!(ring.flags(), 0);
        assert_eq!(ring.idx(), 3);
        assert_eq!(ring.head(0), 10);
        assert_eq!(ring.head(1), 20);
        assert_eq!(ring.head(2), 30);
        assert_eq!(ring.used_event(), 7);
    }

    #[test]
    fn avail_ring_head_indexes_modulo_qsize() {
        // Slot 4 mod qsize=4 == slot 0. Pinning the wraparound semantics.
        let buf = avail_buf(4, 0, &[42, 0, 0, 0], 0);
        let ring = AvailRing::new(&buf, 4).unwrap();
        assert_eq!(ring.head(0), 42);
        assert_eq!(ring.head(4), 42);
        assert_eq!(ring.head(8), 42);
    }

    #[test]
    fn avail_ring_iter_new_yields_heads_since_last_seen() {
        let buf = avail_buf(8, 5, &[10, 11, 12, 13, 14, 0, 0, 0], 0);
        let ring = AvailRing::new(&buf, 8).unwrap();
        let new: Vec<_> = ring.iter_new(0).collect();
        assert_eq!(new, vec![10, 11, 12, 13, 14]);
        let none: Vec<_> = ring.iter_new(5).collect();
        assert!(none.is_empty(), "no new heads since last_seen == idx");
        let two: Vec<_> = ring.iter_new(3).collect();
        assert_eq!(two, vec![13, 14]);
    }

    #[test]
    fn avail_ring_iter_new_handles_idx_wraparound() {
        // Driver has wrapped past u16::MAX once. last_seen=u16::MAX-1,
        // idx=2 means 4 entries written: at slots
        // (u16::MAX-1, u16::MAX, 0, 1) all mod qsize.
        let qsize = 4u16;
        let mut buf = vec![0u8; avail_ring_size(qsize)];
        let idx: u16 = 2;
        buf[2..4].copy_from_slice(&idx.to_le_bytes());
        // Heads stored at slots determined by mod-qsize wraparound:
        let placements = [
            ((u16::MAX - 1) % qsize, 0xA1u16),
            (u16::MAX % qsize, 0xA2),
            (0 % qsize, 0xA3),
            (1 % qsize, 0xA4),
        ];
        for (slot, head) in placements {
            let off = 4 + 2 * slot as usize;
            buf[off..off + 2].copy_from_slice(&head.to_le_bytes());
        }
        let ring = AvailRing::new(&buf, qsize).unwrap();
        let new: Vec<_> = ring.iter_new(u16::MAX - 1).collect();
        assert_eq!(new, vec![0xA1, 0xA2, 0xA3, 0xA4]);
    }

    // ---- UsedRing ------------------------------------------------------

    #[test]
    fn used_ring_push_writes_elem_and_advances_idx() {
        let qsize = 8u16;
        let mut buf = vec![0u8; used_ring_size(qsize)];
        let mut ring = UsedRing::new(&mut buf, qsize).unwrap();
        assert_eq!(ring.idx(), 0);
        let slot = ring.push(7, 256);
        assert_eq!(slot, 0);
        assert_eq!(ring.idx(), 1);
        assert_eq!(ring.elem(0), (7, 256));
        let slot = ring.push(13, 512);
        assert_eq!(slot, 1);
        assert_eq!(ring.idx(), 2);
        assert_eq!(ring.elem(1), (13, 512));
    }

    #[test]
    fn used_ring_push_wraps_slot_at_qsize() {
        let qsize = 4u16;
        let mut buf = vec![0u8; used_ring_size(qsize)];
        let mut ring = UsedRing::new(&mut buf, qsize).unwrap();
        for i in 0..6 {
            // 6 pushes into a 4-slot ring. Slots reused after qsize.
            ring.push(i as u32, i as u32 * 10);
        }
        assert_eq!(ring.idx(), 6);
        // Slot 0 was overwritten by push #4.
        assert_eq!(ring.elem(0), (4, 40));
        // Slot 1 was overwritten by push #5.
        assert_eq!(ring.elem(1), (5, 50));
        // Slots 2/3 still hold pushes #2 and #3.
        assert_eq!(ring.elem(2), (2, 20));
        assert_eq!(ring.elem(3), (3, 30));
    }

    #[test]
    fn used_ring_idx_wraps_at_u16_boundary() {
        let qsize = 4u16;
        let mut buf = vec![0u8; used_ring_size(qsize)];
        let mut ring = UsedRing::new(&mut buf, qsize).unwrap();
        ring.set_idx(u16::MAX);
        ring.push(99, 99);
        assert_eq!(ring.idx(), 0, "idx must wrap u16::MAX -> 0");
    }

    #[test]
    fn used_ring_flags_and_avail_event_roundtrip() {
        let qsize = 16u16;
        let mut buf = vec![0u8; used_ring_size(qsize)];
        let mut ring = UsedRing::new(&mut buf, qsize).unwrap();
        ring.set_flags(VIRTQ_USED_F_NO_NOTIFY);
        ring.set_avail_event(0xCAFE);
        assert_eq!(ring.flags(), VIRTQ_USED_F_NO_NOTIFY);
        assert_eq!(ring.avail_event(), 0xCAFE);
    }

    #[test]
    fn ring_flag_constants_match_virtio_spec() {
        // Pinned by the spec.
        assert_eq!(VIRTQ_AVAIL_F_NO_INTERRUPT, 1);
        assert_eq!(VIRTQ_USED_F_NO_NOTIFY, 1);
    }

    #[test]
    fn used_ring_field_offsets_match_spec() {
        // qsize=2 → flags(0..2), idx(2..4), elem0(4..12), elem1(12..20),
        // avail_event(20..22). Total 22 bytes.
        let qsize = 2u16;
        let mut buf = vec![0u8; used_ring_size(qsize)];
        assert_eq!(buf.len(), 22);
        let mut ring = UsedRing::new(&mut buf, qsize).unwrap();
        ring.set_flags(0x0102);
        ring.set_idx(0x0304);
        ring.push(0x0A0B0C0D, 0x10111213); // writes into slot 0, advances idx
        ring.set_avail_event(0xFEFE);
        // Reconstruct expected bytes manually.
        assert_eq!(&buf[0..2], &0x0102u16.to_le_bytes());
        // After set_idx then push, idx == 0x0305.
        assert_eq!(&buf[2..4], &0x0305u16.to_le_bytes());
        assert_eq!(&buf[4..8], &0x0A0B0C0Du32.to_le_bytes());
        assert_eq!(&buf[8..12], &0x10111213u32.to_le_bytes());
        assert_eq!(&buf[20..22], &0xFEFEu16.to_le_bytes());
    }
}
