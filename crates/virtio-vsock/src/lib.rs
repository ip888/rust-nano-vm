//! virtio-vsock transport for `agent-sandbox-proto`.
//!
//! Scope: **M2**. This crate grows across several PRs. What's here today:
//!
//! - The [`VsockHeader`] packet header (44-byte wire-format struct per the
//!   virtio 1.3 spec §5.10) with `from_bytes` / `write_to` helpers.
//! - [`VsockOp`] and [`VsockType`] enums covering every op/type the spec
//!   defines, plus well-known cids ([`HOST_CID`] & friends) and the
//!   shutdown flags.
//! - [`VsockError`] for parse failures.
//!
//! Deferred to follow-up PRs: the virtqueue consumer loop, connection
//! state machine, packet buffer pool, vm-kvm wiring.
//!
//! # Wire format
//!
//! Quoting `include/uapi/linux/virtio_vsock.h`:
//!
//! ```c
//! struct virtio_vsock_hdr {
//!     __le64 src_cid;
//!     __le64 dst_cid;
//!     __le32 src_port;
//!     __le32 dst_port;
//!     __le32 len;
//!     __le16 type;
//!     __le16 op;
//!     __le32 flags;
//!     __le32 buf_alloc;
//!     __le32 fwd_cnt;
//! };
//! ```
//!
//! Total size: **44 bytes**, all little-endian. The virtio spec pins these
//! byte offsets, so [`VsockHeader::from_bytes`] / [`VsockHeader::write_to`]
//! are the only way values cross this boundary — we never expose a
//! `#[repr(C)]` cast.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use thiserror::Error;

/// On-the-wire size of the virtio-vsock packet header in bytes.
pub const VSOCK_HDR_LEN: usize = 44;

/// Well-known context id: the host-side hypervisor.
pub const HYPERVISOR_CID: u64 = 0;

/// Well-known context id: loopback inside the same endpoint.
pub const LOCAL_CID: u64 = 1;

/// Well-known context id: the host itself (opposite side of the guest).
pub const HOST_CID: u64 = 2;

/// Wildcard context id meaning "any".
pub const ANY_CID: u64 = 0xffff_ffff;

/// Shutdown flags passed with [`VsockOp::Shutdown`].
pub mod shutdown {
    /// Peer will not receive any more data.
    pub const RCV: u32 = 1 << 0;
    /// Peer will not send any more data.
    pub const SEND: u32 = 1 << 1;
}

/// Transport type for a vsock packet.
///
/// v1 only ever emits [`VsockType::Stream`] — the spec also reserves
/// `SeqPacket = 2` and `Dgram = 3`, which we reject on ingress until we
/// need them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum VsockType {
    /// Reliable, ordered, byte-stream (TCP-like). The only type this host
    /// speaks.
    Stream = 1,
}

impl VsockType {
    /// Parse from the raw little-endian `type` field.
    pub fn from_raw(raw: u16) -> Result<Self, VsockError> {
        match raw {
            1 => Ok(Self::Stream),
            other => Err(VsockError::UnknownType(other)),
        }
    }
}

/// Operation code in a vsock packet. Values mirror
/// `include/uapi/linux/virtio_vsock.h`.
///
/// The enum is `#[non_exhaustive]` because the spec permits new ops in
/// future revisions; callers matching on it must include a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
#[non_exhaustive]
pub enum VsockOp {
    /// Reserved / invalid — appears only if a peer sends a zero op.
    Invalid = 0,
    /// Open a new connection.
    Request = 1,
    /// Accept a connection request.
    Response = 2,
    /// Abort a connection.
    Rst = 3,
    /// Half-close with direction flags (see [`shutdown`]).
    Shutdown = 4,
    /// Data payload follows in the packet body (length in `hdr.len`).
    Rw = 5,
    /// Unsolicited credit update (for flow control).
    CreditUpdate = 6,
    /// Explicit request for a credit update from the peer.
    CreditRequest = 7,
}

impl VsockOp {
    /// Parse from the raw little-endian `op` field.
    pub fn from_raw(raw: u16) -> Result<Self, VsockError> {
        match raw {
            0 => Ok(Self::Invalid),
            1 => Ok(Self::Request),
            2 => Ok(Self::Response),
            3 => Ok(Self::Rst),
            4 => Ok(Self::Shutdown),
            5 => Ok(Self::Rw),
            6 => Ok(Self::CreditUpdate),
            7 => Ok(Self::CreditRequest),
            other => Err(VsockError::UnknownOp(other)),
        }
    }

    /// Raw on-the-wire value.
    pub fn as_raw(self) -> u16 {
        self as u16
    }
}

/// Fully decoded virtio-vsock packet header.
///
/// Multi-byte fields are stored in the host's native layout; conversion
/// to/from the on-the-wire little-endian bytes happens in
/// [`Self::from_bytes`] and [`Self::write_to`]. Never transmute a byte
/// buffer into this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsockHeader {
    /// Source context id.
    pub src_cid: u64,
    /// Destination context id.
    pub dst_cid: u64,
    /// Source port.
    pub src_port: u32,
    /// Destination port.
    pub dst_port: u32,
    /// Payload length in bytes (0 for control packets).
    pub len: u32,
    /// Transport type.
    pub vtype: VsockType,
    /// Operation code.
    pub op: VsockOp,
    /// Op-specific flags (e.g. shutdown direction bits).
    pub flags: u32,
    /// Peer-advertised receive buffer size, for flow control.
    pub buf_alloc: u32,
    /// Bytes the peer has consumed, for flow control.
    pub fwd_cnt: u32,
}

impl VsockHeader {
    /// Parse a little-endian header from the first [`VSOCK_HDR_LEN`] bytes
    /// of `buf`. `buf` must be **at least** that long; trailing bytes
    /// (typically the packet payload) are ignored by this call — parse them
    /// separately using the `len` field this returns.
    ///
    /// Returns `Err` if any enum field carries a value the spec doesn't
    /// define or we don't yet support.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, VsockError> {
        if buf.len() < VSOCK_HDR_LEN {
            return Err(VsockError::ShortHeader {
                have: buf.len(),
                need: VSOCK_HDR_LEN,
            });
        }
        // Offsets are pinned by the spec — any change is a wire break.
        let src_cid = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let dst_cid = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let src_port = u32::from_le_bytes(buf[16..20].try_into().unwrap());
        let dst_port = u32::from_le_bytes(buf[20..24].try_into().unwrap());
        let len = u32::from_le_bytes(buf[24..28].try_into().unwrap());
        let type_raw = u16::from_le_bytes(buf[28..30].try_into().unwrap());
        let op_raw = u16::from_le_bytes(buf[30..32].try_into().unwrap());
        let flags = u32::from_le_bytes(buf[32..36].try_into().unwrap());
        let buf_alloc = u32::from_le_bytes(buf[36..40].try_into().unwrap());
        let fwd_cnt = u32::from_le_bytes(buf[40..44].try_into().unwrap());
        Ok(Self {
            src_cid,
            dst_cid,
            src_port,
            dst_port,
            len,
            vtype: VsockType::from_raw(type_raw)?,
            op: VsockOp::from_raw(op_raw)?,
            flags,
            buf_alloc,
            fwd_cnt,
        })
    }

    /// Serialize to `buf`, which must be at least [`VSOCK_HDR_LEN`] bytes.
    /// Writes exactly [`VSOCK_HDR_LEN`] bytes and returns that count.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, VsockError> {
        if buf.len() < VSOCK_HDR_LEN {
            return Err(VsockError::ShortBuffer {
                have: buf.len(),
                need: VSOCK_HDR_LEN,
            });
        }
        buf[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        buf[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        buf[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        buf[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        buf[24..28].copy_from_slice(&self.len.to_le_bytes());
        buf[28..30].copy_from_slice(&(self.vtype as u16).to_le_bytes());
        buf[30..32].copy_from_slice(&self.op.as_raw().to_le_bytes());
        buf[32..36].copy_from_slice(&self.flags.to_le_bytes());
        buf[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        buf[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        Ok(VSOCK_HDR_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; VSOCK_HDR_LEN] {
        let mut out = [0u8; VSOCK_HDR_LEN];
        // Cannot fail: we just allocated exactly VSOCK_HDR_LEN bytes. If
        // write_to ever grows a validation step it will trip this loudly.
        self.write_to(&mut out)
            .expect("serializing VsockHeader into a fixed-size buffer must succeed");
        out
    }
}

/// Errors produced by parsing or building vsock frames.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum VsockError {
    /// Byte slice presented for parsing is smaller than a header.
    #[error("vsock header too short: have {have} bytes, need {need}")]
    ShortHeader {
        /// Bytes we were handed.
        have: usize,
        /// Bytes the header requires.
        need: usize,
    },
    /// Output buffer presented for serialization is smaller than a header.
    #[error("vsock output buffer too small: have {have} bytes, need {need}")]
    ShortBuffer {
        /// Bytes in the output buffer.
        have: usize,
        /// Bytes the serialized header requires.
        need: usize,
    },
    /// `op` field carried a value the spec doesn't define.
    #[error("unknown vsock op {0}")]
    UnknownOp(u16),
    /// `type` field carried a value we don't handle (SeqPacket, Dgram, ...).
    #[error("unsupported vsock type {0}")]
    UnknownType(u16),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> VsockHeader {
        VsockHeader {
            src_cid: HOST_CID,
            dst_cid: 42,
            src_port: 1234,
            dst_port: 5678,
            len: 16,
            vtype: VsockType::Stream,
            op: VsockOp::Rw,
            flags: 0,
            buf_alloc: 262_144,
            fwd_cnt: 1024,
        }
    }

    #[test]
    fn header_length_is_exactly_44() {
        assert_eq!(VSOCK_HDR_LEN, 44);
    }

    #[test]
    fn roundtrip_preserves_every_field() {
        let h = sample();
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), VSOCK_HDR_LEN);
        let decoded = VsockHeader::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, h);
    }

    #[test]
    fn field_offsets_match_virtio_spec() {
        // Build a header whose every field is a distinct marker byte so we
        // can spot-check offsets. Any reorder breaks the wire contract.
        let h = VsockHeader {
            src_cid: 0x0101_0101_0101_0101,
            dst_cid: 0x0202_0202_0202_0202,
            src_port: 0x0303_0303,
            dst_port: 0x0404_0404,
            len: 0x0505_0505,
            vtype: VsockType::Stream, // 0x0001
            op: VsockOp::Rw,          // 0x0005
            flags: 0x0606_0606,
            buf_alloc: 0x0707_0707,
            fwd_cnt: 0x0808_0808,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..8], &[0x01; 8]);
        assert_eq!(&b[8..16], &[0x02; 8]);
        assert_eq!(&b[16..20], &[0x03; 4]);
        assert_eq!(&b[20..24], &[0x04; 4]);
        assert_eq!(&b[24..28], &[0x05; 4]);
        assert_eq!(&b[28..30], &[0x01, 0x00]); // type = Stream = 1 LE
        assert_eq!(&b[30..32], &[0x05, 0x00]); // op = Rw = 5 LE
        assert_eq!(&b[32..36], &[0x06; 4]);
        assert_eq!(&b[36..40], &[0x07; 4]);
        assert_eq!(&b[40..44], &[0x08; 4]);
    }

    #[test]
    fn from_bytes_rejects_short_input() {
        let too_short = [0u8; VSOCK_HDR_LEN - 1];
        let err = VsockHeader::from_bytes(&too_short).unwrap_err();
        assert_eq!(
            err,
            VsockError::ShortHeader {
                have: VSOCK_HDR_LEN - 1,
                need: VSOCK_HDR_LEN,
            }
        );
    }

    #[test]
    fn from_bytes_accepts_longer_buffer_and_ignores_trailing_payload() {
        // Real vsock frames arrive as (header || payload) in a single buffer.
        // `from_bytes` should parse the header and leave payload handling to
        // the caller, not reject the longer buffer.
        let header = sample().to_bytes();
        let mut packet = Vec::with_capacity(header.len() + 32);
        packet.extend_from_slice(&header);
        packet.extend_from_slice(&[0xAB; 32]); // simulated payload
        let decoded = VsockHeader::from_bytes(&packet).expect("longer buffer must parse");
        assert_eq!(decoded, sample());
    }

    #[test]
    fn from_bytes_rejects_unknown_op() {
        let mut bytes = sample().to_bytes();
        bytes[30..32].copy_from_slice(&0xffffu16.to_le_bytes());
        let err = VsockHeader::from_bytes(&bytes).unwrap_err();
        assert_eq!(err, VsockError::UnknownOp(0xffff));
    }

    #[test]
    fn from_bytes_rejects_unsupported_type() {
        let mut bytes = sample().to_bytes();
        // SeqPacket = 2 is spec-defined but we don't support it yet.
        bytes[28..30].copy_from_slice(&2u16.to_le_bytes());
        let err = VsockHeader::from_bytes(&bytes).unwrap_err();
        assert_eq!(err, VsockError::UnknownType(2));
    }

    #[test]
    fn write_to_rejects_short_output_buffer() {
        let h = sample();
        let mut buf = [0u8; 10];
        let err = h.write_to(&mut buf).unwrap_err();
        assert_eq!(
            err,
            VsockError::ShortBuffer {
                have: 10,
                need: VSOCK_HDR_LEN,
            }
        );
    }

    #[test]
    fn op_raw_roundtrip_covers_every_variant() {
        let all = [
            VsockOp::Invalid,
            VsockOp::Request,
            VsockOp::Response,
            VsockOp::Rst,
            VsockOp::Shutdown,
            VsockOp::Rw,
            VsockOp::CreditUpdate,
            VsockOp::CreditRequest,
        ];
        for op in all {
            assert_eq!(VsockOp::from_raw(op.as_raw()).unwrap(), op);
        }
        assert!(matches!(
            VsockOp::from_raw(99),
            Err(VsockError::UnknownOp(99))
        ));
    }

    #[test]
    fn shutdown_flags_are_bit_positions_from_the_spec() {
        // `RCV` = bit 0, `SEND` = bit 1 — pinned by the virtio spec.
        assert_eq!(shutdown::RCV, 1);
        assert_eq!(shutdown::SEND, 2);
        assert_eq!(shutdown::RCV | shutdown::SEND, 3);
    }

    #[test]
    fn well_known_cids_match_the_spec() {
        assert_eq!(HYPERVISOR_CID, 0);
        assert_eq!(LOCAL_CID, 1);
        assert_eq!(HOST_CID, 2);
        assert_eq!(ANY_CID, u32::MAX as u64);
    }
}
