//! Split virtqueue primitives.
//!
//! Scope: what every virtio device (vsock, fs, net, ...) needs in common —
//! the descriptor table entry, flag constants, and a cycle-safe iterator
//! that walks a descriptor chain. Device-specific logic (vsock packet
//! framing, vsock connection state, virtio-fs FUSE framing, ...) lives in
//! each device crate.
//!
//! Deferred to follow-up PRs: the available / used ring parsers, indirect
//! descriptors, packed (vs split) virtqueue, guest-memory integration
//! (`vm-memory`), KVM eventfd plumbing.
//!
//! # Wire format
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
//! 16 bytes per descriptor, all little-endian. Offsets are pinned by the
//! virtio 1.3 spec §2.7.

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
}
