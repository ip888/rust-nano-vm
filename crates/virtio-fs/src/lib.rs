//! virtio-fs host ↔ guest filesystem sharing — FUSE protocol framing.
//!
//! Scope: **M3**. This crate ships the wire types for the FUSE protocol
//! that virtio-fs carries over a virtqueue (the queue itself lives in
//! `virtio-queue`). What lands today:
//!
//! - [`FuseInHeader`] — 40-byte request header (guest → host).
//! - [`FuseOutHeader`] — 16-byte response header (host → guest).
//! - [`FuseOpcode`] — enum covering the FUSE ops we plan to handle in M3.
//! - [`FuseError`] for parse / serialize failures.
//! - Spec constants: [`FUSE_KERNEL_VERSION`], [`FUSE_KERNEL_MINOR_VERSION`],
//!   [`FUSE_IN_HDR_LEN`], [`FUSE_OUT_HDR_LEN`].
//!
//! Deferred to follow-up PRs: per-op request/response body structs
//! (`fuse_init_in`, `fuse_attr`, `fuse_open_out`, ...), the dispatch loop
//! that reads a [`FuseInHeader`] from a virtqueue descriptor chain,
//! invokes the right handler, and writes a [`FuseOutHeader`] + body back.
//!
//! # Wire format
//!
//! Quoting `include/uapi/linux/fuse.h`:
//!
//! ```c
//! struct fuse_in_header {
//!     uint32_t len;            // total length including header
//!     uint32_t opcode;         // FUSE_*
//!     uint64_t unique;         // unique request id, echoed in out_header
//!     uint64_t nodeid;         // target inode
//!     uint32_t uid;
//!     uint32_t gid;
//!     uint32_t pid;
//!     uint16_t total_extlen;   // length of extensions (8-byte units)
//!     uint16_t padding;
//! };
//!
//! struct fuse_out_header {
//!     uint32_t len;
//!     int32_t  error;          // negative errno on failure, 0 on success
//!     uint64_t unique;         // copied from request
//! };
//! ```
//!
//! All multi-byte fields are little-endian on x86_64 (FUSE inherits the
//! host byte order; we only target LE host + LE guest, so we treat the
//! wire as LE unconditionally and pin offsets via tests).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use thiserror::Error;

/// FUSE major protocol version this build implements. Bumped lock-step
/// with the Linux FUSE kernel; the guest's `init` op carries its own
/// version and the host MUST refuse a guest that's incompatible.
pub const FUSE_KERNEL_VERSION: u32 = 7;

/// FUSE minor protocol version this build implements. v7.33 is the
/// minimum the upstream `virtiofsd` crate-tree settled on for "modern"
/// virtio-fs; ours is set to match so we can negotiate without forcing
/// the guest to fall back.
pub const FUSE_KERNEL_MINOR_VERSION: u32 = 33;

/// On-the-wire size of [`FuseInHeader`] in bytes.
pub const FUSE_IN_HDR_LEN: usize = 40;

/// On-the-wire size of [`FuseOutHeader`] in bytes.
pub const FUSE_OUT_HDR_LEN: usize = 16;

// ---------------------------------------------------------------------------
// Opcodes
// ---------------------------------------------------------------------------

/// FUSE operation code as carried in [`FuseInHeader::opcode`].
///
/// The enum is `#[non_exhaustive]` because the FUSE wire protocol grows
/// over kernel releases; callers matching on it must include a wildcard
/// arm so a future kernel sending an unfamiliar op cannot panic the host.
///
/// Variants are numbered to match `FUSE_*` constants in
/// `include/uapi/linux/fuse.h`. Only the ops we plan to implement in M3
/// are listed; new ones go at the end (they're explicitly assigned, not
/// auto-incremented, so adding a variant in the middle would not change
/// existing wire values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
#[non_exhaustive]
pub enum FuseOpcode {
    /// `FUSE_LOOKUP` — resolve a path component within `nodeid`.
    Lookup = 1,
    /// `FUSE_FORGET` — drop a kernel reference to an inode.
    Forget = 2,
    /// `FUSE_GETATTR` — read inode attributes.
    Getattr = 3,
    /// `FUSE_SETATTR` — modify inode attributes (chmod / chown / truncate).
    Setattr = 4,
    /// `FUSE_READLINK` — read a symlink target.
    Readlink = 5,
    /// `FUSE_SYMLINK` — create a symlink.
    Symlink = 6,
    /// `FUSE_MKNOD` — create a special file (rarely needed for our use).
    Mknod = 8,
    /// `FUSE_MKDIR` — create a directory.
    Mkdir = 9,
    /// `FUSE_UNLINK` — remove a file.
    Unlink = 10,
    /// `FUSE_RMDIR` — remove an empty directory.
    Rmdir = 11,
    /// `FUSE_RENAME` — rename / move within the same parent.
    Rename = 12,
    /// `FUSE_LINK` — hardlink an inode under a new name.
    Link = 13,
    /// `FUSE_OPEN` — open a regular file, returning a file handle.
    Open = 14,
    /// `FUSE_READ` — read from an open file handle.
    Read = 15,
    /// `FUSE_WRITE` — write to an open file handle.
    Write = 16,
    /// `FUSE_STATFS` — vfs `statfs(2)` on the mount.
    Statfs = 17,
    /// `FUSE_RELEASE` — close a file handle returned by `Open`.
    Release = 18,
    /// `FUSE_FSYNC` — flush a file handle to backing store.
    Fsync = 20,
    /// `FUSE_FLUSH` — flush write buffer for a file handle on close().
    Flush = 25,
    /// `FUSE_INIT` — protocol handshake; first packet on every mount.
    Init = 26,
    /// `FUSE_OPENDIR` — open a directory, returning a handle for `Readdir`.
    Opendir = 27,
    /// `FUSE_READDIR` — list directory entries.
    Readdir = 28,
    /// `FUSE_RELEASEDIR` — close a directory handle from `Opendir`.
    Releasedir = 29,
    /// `FUSE_DESTROY` — orderly shutdown of the FUSE session.
    Destroy = 38,
}

impl FuseOpcode {
    /// Parse from the raw little-endian opcode value. Returns
    /// [`FuseError::UnknownOpcode`] for any value the spec defines but we
    /// don't (yet) handle, *and* for any value the spec doesn't define.
    /// Callers who want lenient handling can intercept the error and
    /// reply with `-ENOSYS`.
    pub fn from_raw(raw: u32) -> Result<Self, FuseError> {
        match raw {
            1 => Ok(Self::Lookup),
            2 => Ok(Self::Forget),
            3 => Ok(Self::Getattr),
            4 => Ok(Self::Setattr),
            5 => Ok(Self::Readlink),
            6 => Ok(Self::Symlink),
            8 => Ok(Self::Mknod),
            9 => Ok(Self::Mkdir),
            10 => Ok(Self::Unlink),
            11 => Ok(Self::Rmdir),
            12 => Ok(Self::Rename),
            13 => Ok(Self::Link),
            14 => Ok(Self::Open),
            15 => Ok(Self::Read),
            16 => Ok(Self::Write),
            17 => Ok(Self::Statfs),
            18 => Ok(Self::Release),
            20 => Ok(Self::Fsync),
            25 => Ok(Self::Flush),
            26 => Ok(Self::Init),
            27 => Ok(Self::Opendir),
            28 => Ok(Self::Readdir),
            29 => Ok(Self::Releasedir),
            38 => Ok(Self::Destroy),
            other => Err(FuseError::UnknownOpcode(other)),
        }
    }

    /// Raw on-the-wire value for this opcode.
    pub fn as_raw(self) -> u32 {
        self as u32
    }
}

// ---------------------------------------------------------------------------
// fuse_in_header
// ---------------------------------------------------------------------------

/// Decoded `fuse_in_header`. Multi-byte fields stored in host native
/// layout; cross the wire boundary via [`Self::from_bytes`] /
/// [`Self::write_to`] only. Never `#[repr(C)]` cast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseInHeader {
    /// Total request length in bytes including this header. The body
    /// follows immediately and is `len - FUSE_IN_HDR_LEN` bytes.
    pub len: u32,
    /// Operation to perform.
    pub opcode: FuseOpcode,
    /// Caller-chosen request id, echoed in the matching
    /// [`FuseOutHeader::unique`].
    pub unique: u64,
    /// Target inode (`nodeid`). For per-mount ops (`Init`, `Statfs`,
    /// `Destroy`) callers may pass `0` or `1` (root) per the spec; this
    /// crate doesn't validate.
    pub nodeid: u64,
    /// Caller's user id.
    pub uid: u32,
    /// Caller's group id.
    pub gid: u32,
    /// Caller's process id.
    pub pid: u32,
    /// Length of header extensions in 8-byte units. `0` for the protocol
    /// versions we target; non-zero would mean post-header extension data
    /// before the request body.
    pub total_extlen: u16,
    /// Padding bytes; writers MUST emit `0`. Readers MUST ignore so a
    /// future kernel can repurpose them.
    pub padding: u16,
}

impl FuseInHeader {
    /// Parse the first [`FUSE_IN_HDR_LEN`] bytes of `buf` as an
    /// in-header. `buf` must be at least that long; trailing bytes form
    /// the request body and are not consumed by this call.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_IN_HDR_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_IN_HDR_LEN,
            });
        }
        let len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let opcode_raw = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let unique = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let nodeid = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        let uid = u32::from_le_bytes(buf[24..28].try_into().unwrap());
        let gid = u32::from_le_bytes(buf[28..32].try_into().unwrap());
        let pid = u32::from_le_bytes(buf[32..36].try_into().unwrap());
        let total_extlen = u16::from_le_bytes(buf[36..38].try_into().unwrap());
        let padding = u16::from_le_bytes(buf[38..40].try_into().unwrap());
        Ok(Self {
            len,
            opcode: FuseOpcode::from_raw(opcode_raw)?,
            unique,
            nodeid,
            uid,
            gid,
            pid,
            total_extlen,
            padding,
        })
    }

    /// Serialize into `buf` (must be at least [`FUSE_IN_HDR_LEN`] bytes).
    /// Writes exactly [`FUSE_IN_HDR_LEN`] bytes and returns that count.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_IN_HDR_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_IN_HDR_LEN,
            });
        }
        buf[0..4].copy_from_slice(&self.len.to_le_bytes());
        buf[4..8].copy_from_slice(&self.opcode.as_raw().to_le_bytes());
        buf[8..16].copy_from_slice(&self.unique.to_le_bytes());
        buf[16..24].copy_from_slice(&self.nodeid.to_le_bytes());
        buf[24..28].copy_from_slice(&self.uid.to_le_bytes());
        buf[28..32].copy_from_slice(&self.gid.to_le_bytes());
        buf[32..36].copy_from_slice(&self.pid.to_le_bytes());
        buf[36..38].copy_from_slice(&self.total_extlen.to_le_bytes());
        buf[38..40].copy_from_slice(&self.padding.to_le_bytes());
        Ok(FUSE_IN_HDR_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_IN_HDR_LEN] {
        let mut out = [0u8; FUSE_IN_HDR_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseInHeader into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_out_header
// ---------------------------------------------------------------------------

/// Decoded `fuse_out_header`.
///
/// `error` is signed: `0` for success, negative errno (e.g. `-libc::ENOENT`)
/// for failures. We don't pre-encode error semantics here — the dispatcher
/// crate decides what to put in this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseOutHeader {
    /// Total response length in bytes including this header.
    pub len: u32,
    /// `0` on success, negative errno on failure.
    pub error: i32,
    /// Copied from the matching [`FuseInHeader::unique`].
    pub unique: u64,
}

impl FuseOutHeader {
    /// Build a success response header for a body of `body_len` bytes.
    pub fn ok(unique: u64, body_len: u32) -> Self {
        Self {
            len: FUSE_OUT_HDR_LEN as u32 + body_len,
            error: 0,
            unique,
        }
    }

    /// Build an error response header. `errno` should be a *positive*
    /// errno value; this constructor flips the sign for the wire field.
    /// Body is implicitly empty (no payload after the header).
    pub fn err(unique: u64, errno: u32) -> Self {
        Self {
            len: FUSE_OUT_HDR_LEN as u32,
            error: -(errno as i32),
            unique,
        }
    }

    /// Parse the first [`FUSE_OUT_HDR_LEN`] bytes of `buf`.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_OUT_HDR_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_OUT_HDR_LEN,
            });
        }
        Ok(Self {
            len: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            error: i32::from_le_bytes(buf[4..8].try_into().unwrap()),
            unique: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
        })
    }

    /// Serialize into `buf` (must be at least [`FUSE_OUT_HDR_LEN`] bytes).
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_OUT_HDR_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_OUT_HDR_LEN,
            });
        }
        buf[0..4].copy_from_slice(&self.len.to_le_bytes());
        buf[4..8].copy_from_slice(&self.error.to_le_bytes());
        buf[8..16].copy_from_slice(&self.unique.to_le_bytes());
        Ok(FUSE_OUT_HDR_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_OUT_HDR_LEN] {
        let mut out = [0u8; FUSE_OUT_HDR_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseOutHeader into a fixed-size buffer must succeed");
        out
    }

    /// `true` when this header reports a failure (`error != 0`).
    pub fn is_error(&self) -> bool {
        self.error != 0
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors produced by parsing or building FUSE frames.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum FuseError {
    /// Input byte slice is smaller than the requested header.
    #[error("fuse header too short: have {have} bytes, need {need}")]
    ShortHeader {
        /// Bytes we were handed.
        have: usize,
        /// Bytes the header requires.
        need: usize,
    },
    /// Output byte slice is smaller than the requested header.
    #[error("fuse output buffer too small: have {have} bytes, need {need}")]
    ShortBuffer {
        /// Bytes in the output buffer.
        have: usize,
        /// Bytes the serialized header requires.
        need: usize,
    },
    /// `opcode` field carried a value the spec doesn't define, or one the
    /// spec defines but this crate doesn't yet handle. The dispatcher
    /// should reply with `-ENOSYS` in either case.
    #[error("unknown or unsupported fuse opcode {0}")]
    UnknownOpcode(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_in() -> FuseInHeader {
        FuseInHeader {
            len: FUSE_IN_HDR_LEN as u32 + 16,
            opcode: FuseOpcode::Read,
            unique: 0x0102_0304_0506_0708,
            nodeid: 0x42,
            uid: 1000,
            gid: 1000,
            pid: 12345,
            total_extlen: 0,
            padding: 0,
        }
    }

    fn sample_out() -> FuseOutHeader {
        FuseOutHeader::ok(0x0102_0304_0506_0708, 16)
    }

    // ---- Constants ----------------------------------------------------

    #[test]
    fn header_lengths_match_spec() {
        assert_eq!(FUSE_IN_HDR_LEN, 40);
        assert_eq!(FUSE_OUT_HDR_LEN, 16);
    }

    #[test]
    fn protocol_version_is_pinned() {
        // Pinned so a careless bump trips a test.
        assert_eq!(FUSE_KERNEL_VERSION, 7);
        assert_eq!(FUSE_KERNEL_MINOR_VERSION, 33);
    }

    // ---- FuseInHeader -------------------------------------------------

    #[test]
    fn in_header_roundtrips_every_field() {
        let h = sample_in();
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), FUSE_IN_HDR_LEN);
        let back = FuseInHeader::from_bytes(&bytes).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn in_header_field_offsets_match_spec() {
        // Distinct values per field so a swap or shifted offset would
        // produce a recognizable mismatch.
        let h = FuseInHeader {
            len: 0x0101_0101,
            opcode: FuseOpcode::Lookup, // raw = 1
            unique: 0x0303_0303_0303_0303,
            nodeid: 0x0404_0404_0404_0404,
            uid: 0x0505_0505,
            gid: 0x0606_0606,
            pid: 0x0707_0707,
            total_extlen: 0x0808,
            padding: 0x0909,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..4], &[0x01; 4]);
        assert_eq!(&b[4..8], &[0x01, 0x00, 0x00, 0x00]); // opcode = 1 LE
        assert_eq!(&b[8..16], &[0x03; 8]);
        assert_eq!(&b[16..24], &[0x04; 8]);
        assert_eq!(&b[24..28], &[0x05; 4]);
        assert_eq!(&b[28..32], &[0x06; 4]);
        assert_eq!(&b[32..36], &[0x07; 4]);
        assert_eq!(&b[36..38], &[0x08, 0x08]);
        assert_eq!(&b[38..40], &[0x09, 0x09]);
    }

    #[test]
    fn in_header_accepts_longer_buffer_and_ignores_request_body() {
        // (header || body) is the on-the-wire shape; from_bytes must not
        // reject the extra payload.
        let h = sample_in();
        let mut packet = Vec::with_capacity(FUSE_IN_HDR_LEN + 16);
        packet.extend_from_slice(&h.to_bytes());
        packet.extend_from_slice(&[0xAB; 16]); // simulated body
        let back = FuseInHeader::from_bytes(&packet).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn in_header_rejects_short_input() {
        let short = [0u8; FUSE_IN_HDR_LEN - 1];
        let err = FuseInHeader::from_bytes(&short).unwrap_err();
        assert_eq!(
            err,
            FuseError::ShortHeader {
                have: FUSE_IN_HDR_LEN - 1,
                need: FUSE_IN_HDR_LEN,
            }
        );
    }

    #[test]
    fn in_header_rejects_unknown_opcode() {
        let mut bytes = sample_in().to_bytes();
        bytes[4..8].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        let err = FuseInHeader::from_bytes(&bytes).unwrap_err();
        assert_eq!(err, FuseError::UnknownOpcode(0xDEAD_BEEF));
    }

    #[test]
    fn write_to_in_header_rejects_short_output() {
        let h = sample_in();
        let mut buf = [0u8; 8];
        let err = h.write_to(&mut buf).unwrap_err();
        assert_eq!(
            err,
            FuseError::ShortBuffer {
                have: 8,
                need: FUSE_IN_HDR_LEN,
            }
        );
    }

    // ---- FuseOutHeader ------------------------------------------------

    #[test]
    fn out_header_ok_constructor_sets_len_and_zero_error() {
        let h = FuseOutHeader::ok(42, 100);
        assert_eq!(h.unique, 42);
        assert_eq!(h.error, 0);
        assert_eq!(h.len, FUSE_OUT_HDR_LEN as u32 + 100);
        assert!(!h.is_error());
    }

    #[test]
    fn out_header_err_constructor_uses_negative_errno_and_empty_body() {
        // ENOENT = 2 on Linux; we don't import libc so use the literal.
        let h = FuseOutHeader::err(99, 2);
        assert_eq!(h.unique, 99);
        assert_eq!(h.error, -2);
        assert_eq!(h.len, FUSE_OUT_HDR_LEN as u32);
        assert!(h.is_error());
    }

    #[test]
    fn out_header_roundtrips_every_field() {
        let h = sample_out();
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), FUSE_OUT_HDR_LEN);
        let back = FuseOutHeader::from_bytes(&bytes).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn out_header_field_offsets_match_spec() {
        let h = FuseOutHeader {
            len: 0x0101_0101,
            error: -42,
            unique: 0x0202_0202_0202_0202,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..4], &[0x01; 4]);
        assert_eq!(&b[4..8], &(-42i32).to_le_bytes());
        assert_eq!(&b[8..16], &[0x02; 8]);
    }

    #[test]
    fn out_header_accepts_longer_buffer_and_ignores_response_body() {
        let h = sample_out();
        let mut packet = Vec::with_capacity(FUSE_OUT_HDR_LEN + 16);
        packet.extend_from_slice(&h.to_bytes());
        packet.extend_from_slice(&[0xAB; 16]);
        let back = FuseOutHeader::from_bytes(&packet).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn out_header_rejects_short_input() {
        let short = [0u8; FUSE_OUT_HDR_LEN - 1];
        let err = FuseOutHeader::from_bytes(&short).unwrap_err();
        assert_eq!(
            err,
            FuseError::ShortHeader {
                have: FUSE_OUT_HDR_LEN - 1,
                need: FUSE_OUT_HDR_LEN,
            }
        );
    }

    #[test]
    fn write_to_out_header_rejects_short_output() {
        let h = sample_out();
        let mut buf = [0u8; 4];
        let err = h.write_to(&mut buf).unwrap_err();
        assert_eq!(
            err,
            FuseError::ShortBuffer {
                have: 4,
                need: FUSE_OUT_HDR_LEN,
            }
        );
    }

    // ---- FuseOpcode ---------------------------------------------------

    #[test]
    fn opcode_raw_roundtrip_covers_every_variant() {
        let all = [
            FuseOpcode::Lookup,
            FuseOpcode::Forget,
            FuseOpcode::Getattr,
            FuseOpcode::Setattr,
            FuseOpcode::Readlink,
            FuseOpcode::Symlink,
            FuseOpcode::Mknod,
            FuseOpcode::Mkdir,
            FuseOpcode::Unlink,
            FuseOpcode::Rmdir,
            FuseOpcode::Rename,
            FuseOpcode::Link,
            FuseOpcode::Open,
            FuseOpcode::Read,
            FuseOpcode::Write,
            FuseOpcode::Statfs,
            FuseOpcode::Release,
            FuseOpcode::Fsync,
            FuseOpcode::Flush,
            FuseOpcode::Init,
            FuseOpcode::Opendir,
            FuseOpcode::Readdir,
            FuseOpcode::Releasedir,
            FuseOpcode::Destroy,
        ];
        for op in all {
            assert_eq!(FuseOpcode::from_raw(op.as_raw()).unwrap(), op);
        }
        assert!(matches!(
            FuseOpcode::from_raw(99_999),
            Err(FuseError::UnknownOpcode(99_999))
        ));
    }

    #[test]
    fn opcode_values_match_linux_uapi_constants() {
        // Pinned from include/uapi/linux/fuse.h. Any change here breaks
        // wire compatibility with stock kernels.
        assert_eq!(FuseOpcode::Lookup.as_raw(), 1);
        assert_eq!(FuseOpcode::Getattr.as_raw(), 3);
        assert_eq!(FuseOpcode::Open.as_raw(), 14);
        assert_eq!(FuseOpcode::Read.as_raw(), 15);
        assert_eq!(FuseOpcode::Write.as_raw(), 16);
        assert_eq!(FuseOpcode::Init.as_raw(), 26);
        assert_eq!(FuseOpcode::Destroy.as_raw(), 38);
    }
}
