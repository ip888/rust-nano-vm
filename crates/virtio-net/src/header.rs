//! # virtio-net packet header (`struct virtio_net_hdr`)
//!
//! > **Terminology:** every term in this file (TX, RX, GSO, TSO, MTU,
//! > FCS, Ethernet frame, …) is defined once in the crate-root
//! > terminology table — see the top of `lib.rs`.
//!
//! Every frame that crosses a virtio-net queue — guest → host (TX,
//! *transmit*) or host → guest (RX, *receive*) — is prefixed with
//! this 12-byte header. The Ethernet frame starts immediately after.
//! The header carries metadata the hardware would have supplied in a
//! physical NIC: checksum offload hints, segmentation (TSO/UFO) info,
//! and a count of merged RX buffers.
//!
//! On the wire, every TX chain the guest produces and every RX chain
//! we fill looks like:
//!
//! ```text
//!   ┌────────────────────────┬──────────────────────────────────┐
//!   │  virtio-net header     │          Ethernet frame           │
//!   │       12 bytes         │  14 B hdr + up to 1500 B payload │
//!   └────────────────────────┴──────────────────────────────────┘
//!   ◄──────────── 12 bytes ─────────────► ◄── up to 1514 bytes ──►
//! ```
//!
//! On TX the queue consumer **strips** this header before handing the
//! Ethernet frame to the backend. On RX it **prepends** an all-zeros
//! `no_offload()` header to the frame the backend produced. The
//! backend only ever sees raw Ethernet frames — that's the layering
//! boundary between this crate and the transport.
//!
//! Wire format (virtio 1.3 §5.1.6, with VIRTIO_F_VERSION_1 negotiated):
//!
//! ```c
//! struct virtio_net_hdr {
//!     u8  flags;        // VIRTIO_NET_HDR_F_*
//!     u8  gso_type;     // VIRTIO_NET_HDR_GSO_*
//!     __le16 hdr_len;   // length of the L2+L3+L4 header in-frame
//!     __le16 gso_size;  // size per GSO segment (TSO MSS)
//!     __le16 csum_start;    // offset at which to compute checksum
//!     __le16 csum_offset;   // where to write the checksum
//!     __le16 num_buffers;   // (VIRTIO_F_VERSION_1) always present; counts
//!                           //  how many RX descriptors this frame used
//! };
//! ```
//!
//! Total size: **12 bytes**. (Pre-1.0 virtio with `VIRTIO_NET_F_MRG_RXBUF`
//! disabled used 10 bytes; we require VERSION_1, so the 12-byte layout
//! is universal here.)
//!
//! # Why we need it even when we don't offload
//!
//! Our v1 TAP backend doesn't do checksum offload or TSO — the kernel
//! does that on the real NIC behind the bridge. We advertise
//! `flags = 0`, `gso_type = NONE` on every TX; the guest driver
//! likewise sends us "no offload" frames. **But the header is still
//! mandatory**: the virtio spec says every packet starts with it, and
//! Linux's `virtio_net` driver prepends/strips it unconditionally. So
//! this module exists to parse the header off incoming TX frames and
//! prepend an all-zeros header to outgoing RX frames.

/// On-the-wire size of the virtio-net header in bytes (with
/// VIRTIO_F_VERSION_1 negotiated).
pub const VIRTIO_NET_HDR_LEN: usize = 12;

// --- flags bits ------------------------------------------------------------

/// Checksum is partial: driver has set `csum_start` / `csum_offset` and
/// the device must compute+write the one's-complement there.
pub const VIRTIO_NET_HDR_F_NEEDS_CSUM: u8 = 1;

/// Device has already validated the checksum; driver may skip its own
/// verification.
pub const VIRTIO_NET_HDR_F_DATA_VALID: u8 = 2;

/// Receive-side coalescing info is present (RSC — Linux doesn't use
/// this on the transmit side; we advertise = 0).
pub const VIRTIO_NET_HDR_F_RSC_INFO: u8 = 4;

// --- gso_type values -------------------------------------------------------

/// No GSO. The entire packet fits in one segment; no segmentation
/// metadata is needed.
pub const VIRTIO_NET_HDR_GSO_NONE: u8 = 0;

/// IPv4 TCP segmentation. Guest hands us one big TCP segment;
/// hardware/host splits it into MSS-sized packets.
pub const VIRTIO_NET_HDR_GSO_TCPV4: u8 = 1;

/// Reserved (historical: UDP fragmentation offload for IPv4).
pub const VIRTIO_NET_HDR_GSO_UDP: u8 = 3;

/// IPv6 TCP segmentation.
pub const VIRTIO_NET_HDR_GSO_TCPV6: u8 = 4;

/// "Explicit congestion notification" flag OR'd into `gso_type`.
pub const VIRTIO_NET_HDR_GSO_ECN: u8 = 0x80;

/// Fully decoded virtio-net packet header.
///
/// Fields are stored in the host's native layout; conversion to/from
/// the on-the-wire little-endian bytes happens in [`Self::from_bytes`]
/// and [`Self::write_to`]. We never transmute a byte buffer into this
/// type — the crate root is `#![deny(unsafe_code)]` for exactly that
/// reason.
///
/// # Rust concept: `#[derive(...)]`
///
/// This attribute asks the compiler to generate boilerplate trait
/// implementations for us: `Debug` for `{:?}` printing, `Clone` +
/// `Copy` so the struct can be duplicated by value (it's 12 bytes —
/// cheaper than a pointer), `PartialEq` + `Eq` for `==` comparison,
/// and `Default` for `VirtioNetHdr::default()` producing an all-zeros
/// header (the no-offload case). Equivalent hand-written impls would
/// take ~40 lines; `#[derive]` is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioNetHdr {
    /// Bitfield of `VIRTIO_NET_HDR_F_*`.
    pub flags: u8,
    /// GSO/TSO segmentation type (`VIRTIO_NET_HDR_GSO_*`).
    pub gso_type: u8,
    /// Byte length of the in-frame L2+L3+L4 header the hardware should
    /// treat as the per-segment prefix (TSO/UFO only).
    pub hdr_len: u16,
    /// Per-segment payload size (TSO MSS). Zero when `gso_type == NONE`.
    pub gso_size: u16,
    /// Offset into the frame at which the device computes the checksum.
    pub csum_start: u16,
    /// Offset into the frame at which the device writes the checksum.
    pub csum_offset: u16,
    /// RX only: how many descriptor chains the device used to deliver
    /// this frame. Set to 1 by the device for every RX frame; the
    /// driver reads it to know if the frame was split across multiple
    /// chains (`VIRTIO_NET_F_MRG_RXBUF`). We don't advertise MRG_RXBUF
    /// yet, so this is always 1 on RX and ignored on TX.
    pub num_buffers: u16,
}

/// Errors from parsing / serializing a [`VirtioNetHdr`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HeaderError {
    /// Byte slice presented for parsing is shorter than [`VIRTIO_NET_HDR_LEN`].
    #[error("virtio-net header too short: have {have} bytes, need {need}")]
    ShortHeader {
        /// Bytes we were handed.
        have: usize,
        /// Bytes the header requires.
        need: usize,
    },
    /// Output buffer presented for serialization is shorter than
    /// [`VIRTIO_NET_HDR_LEN`].
    #[error("virtio-net output buffer too small: have {have} bytes, need {need}")]
    ShortBuffer {
        /// Bytes in the output buffer.
        have: usize,
        /// Bytes the serialized header requires.
        need: usize,
    },
}

impl VirtioNetHdr {
    /// A no-offload, no-GSO header. The convenience constructor the host
    /// side uses on every outgoing (RX) frame when it doesn't do any
    /// offloading itself.
    pub fn no_offload() -> Self {
        Self {
            flags: 0,
            gso_type: VIRTIO_NET_HDR_GSO_NONE,
            hdr_len: 0,
            gso_size: 0,
            csum_start: 0,
            csum_offset: 0,
            num_buffers: 1,
        }
    }

    /// Parse a little-endian header from the first [`VIRTIO_NET_HDR_LEN`]
    /// bytes of `buf`. Trailing bytes (the Ethernet frame itself) are
    /// ignored by this call — the caller handles the payload.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, HeaderError> {
        if buf.len() < VIRTIO_NET_HDR_LEN {
            return Err(HeaderError::ShortHeader {
                have: buf.len(),
                need: VIRTIO_NET_HDR_LEN,
            });
        }
        // Offsets pinned by the virtio spec — any reorder is a wire break.
        Ok(Self {
            flags: buf[0],
            gso_type: buf[1],
            hdr_len: u16::from_le_bytes([buf[2], buf[3]]),
            gso_size: u16::from_le_bytes([buf[4], buf[5]]),
            csum_start: u16::from_le_bytes([buf[6], buf[7]]),
            csum_offset: u16::from_le_bytes([buf[8], buf[9]]),
            num_buffers: u16::from_le_bytes([buf[10], buf[11]]),
        })
    }

    /// Serialize into `buf`, which must be at least [`VIRTIO_NET_HDR_LEN`]
    /// bytes. Writes exactly [`VIRTIO_NET_HDR_LEN`] bytes and returns
    /// that count.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, HeaderError> {
        if buf.len() < VIRTIO_NET_HDR_LEN {
            return Err(HeaderError::ShortBuffer {
                have: buf.len(),
                need: VIRTIO_NET_HDR_LEN,
            });
        }
        buf[0] = self.flags;
        buf[1] = self.gso_type;
        buf[2..4].copy_from_slice(&self.hdr_len.to_le_bytes());
        buf[4..6].copy_from_slice(&self.gso_size.to_le_bytes());
        buf[6..8].copy_from_slice(&self.csum_start.to_le_bytes());
        buf[8..10].copy_from_slice(&self.csum_offset.to_le_bytes());
        buf[10..12].copy_from_slice(&self.num_buffers.to_le_bytes());
        Ok(VIRTIO_NET_HDR_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; VIRTIO_NET_HDR_LEN] {
        let mut out = [0u8; VIRTIO_NET_HDR_LEN];
        out[0] = self.flags;
        out[1] = self.gso_type;
        out[2..4].copy_from_slice(&self.hdr_len.to_le_bytes());
        out[4..6].copy_from_slice(&self.gso_size.to_le_bytes());
        out[6..8].copy_from_slice(&self.csum_start.to_le_bytes());
        out[8..10].copy_from_slice(&self.csum_offset.to_le_bytes());
        out[10..12].copy_from_slice(&self.num_buffers.to_le_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_length_is_exactly_12() {
        assert_eq!(VIRTIO_NET_HDR_LEN, 12);
    }

    #[test]
    fn no_offload_default_is_all_zeros_with_num_buffers_1() {
        let h = VirtioNetHdr::no_offload();
        assert_eq!(h.flags, 0);
        assert_eq!(h.gso_type, VIRTIO_NET_HDR_GSO_NONE);
        assert_eq!(h.hdr_len, 0);
        assert_eq!(h.gso_size, 0);
        assert_eq!(h.csum_start, 0);
        assert_eq!(h.csum_offset, 0);
        assert_eq!(h.num_buffers, 1);
    }

    #[test]
    fn roundtrip_preserves_every_field() {
        let h = VirtioNetHdr {
            flags: VIRTIO_NET_HDR_F_NEEDS_CSUM,
            gso_type: VIRTIO_NET_HDR_GSO_TCPV4,
            hdr_len: 54,
            gso_size: 1448,
            csum_start: 14,
            csum_offset: 16,
            num_buffers: 1,
        };
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), VIRTIO_NET_HDR_LEN);
        let decoded = VirtioNetHdr::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, h);
    }

    #[test]
    fn field_offsets_match_virtio_spec() {
        // Build a header with distinct marker bytes per field so a byte
        // reorder is caught here, not weeks later by a confused guest.
        let h = VirtioNetHdr {
            flags: 0x11,
            gso_type: 0x22,
            hdr_len: 0x3344,
            gso_size: 0x5566,
            csum_start: 0x7788,
            csum_offset: 0x99aa,
            num_buffers: 0xbbcc,
        };
        let b = h.to_bytes();
        assert_eq!(b[0], 0x11);
        assert_eq!(b[1], 0x22);
        assert_eq!(&b[2..4], &[0x44, 0x33]); // LE
        assert_eq!(&b[4..6], &[0x66, 0x55]);
        assert_eq!(&b[6..8], &[0x88, 0x77]);
        assert_eq!(&b[8..10], &[0xaa, 0x99]);
        assert_eq!(&b[10..12], &[0xcc, 0xbb]);
    }

    #[test]
    fn from_bytes_rejects_short_input() {
        let short = [0u8; VIRTIO_NET_HDR_LEN - 1];
        let err = VirtioNetHdr::from_bytes(&short).unwrap_err();
        assert_eq!(
            err,
            HeaderError::ShortHeader {
                have: VIRTIO_NET_HDR_LEN - 1,
                need: VIRTIO_NET_HDR_LEN,
            }
        );
    }

    #[test]
    fn from_bytes_accepts_longer_buffer_and_ignores_payload() {
        // Real virtio-net frames arrive as (header || payload) in a single
        // buffer. Parsing must not reject the longer buffer.
        let header = VirtioNetHdr::no_offload().to_bytes();
        let mut packet = Vec::with_capacity(header.len() + 1500);
        packet.extend_from_slice(&header);
        packet.extend_from_slice(&[0xAB; 64]); // simulated Ethernet frame
        let decoded = VirtioNetHdr::from_bytes(&packet).expect("longer buffer must parse");
        assert_eq!(decoded, VirtioNetHdr::no_offload());
    }

    #[test]
    fn write_to_rejects_short_output_buffer() {
        let h = VirtioNetHdr::no_offload();
        let mut buf = [0u8; VIRTIO_NET_HDR_LEN - 1];
        let err = h.write_to(&mut buf).unwrap_err();
        assert_eq!(
            err,
            HeaderError::ShortBuffer {
                have: VIRTIO_NET_HDR_LEN - 1,
                need: VIRTIO_NET_HDR_LEN,
            }
        );
    }

    #[test]
    fn write_to_leaves_trailing_bytes_untouched() {
        // Caller passes a buffer sized for header + payload; write_to
        // must only touch the first 12 bytes.
        let h = VirtioNetHdr::no_offload();
        let mut buf = [0xFFu8; VIRTIO_NET_HDR_LEN + 4];
        h.write_to(&mut buf).unwrap();
        assert_eq!(&buf[VIRTIO_NET_HDR_LEN..], &[0xFF; 4]);
    }

    #[test]
    fn gso_constants_match_virtio_spec() {
        assert_eq!(VIRTIO_NET_HDR_GSO_NONE, 0);
        assert_eq!(VIRTIO_NET_HDR_GSO_TCPV4, 1);
        assert_eq!(VIRTIO_NET_HDR_GSO_UDP, 3);
        assert_eq!(VIRTIO_NET_HDR_GSO_TCPV6, 4);
        assert_eq!(VIRTIO_NET_HDR_GSO_ECN, 0x80);
    }

    #[test]
    fn flag_bits_match_virtio_spec() {
        assert_eq!(VIRTIO_NET_HDR_F_NEEDS_CSUM, 1);
        assert_eq!(VIRTIO_NET_HDR_F_DATA_VALID, 2);
        assert_eq!(VIRTIO_NET_HDR_F_RSC_INFO, 4);
    }
}
