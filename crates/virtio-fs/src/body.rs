//! Per-op FUSE request / response body structs.
//!
//! Scope today: the four types needed for the M3 handshake + lookup
//! path:
//!
//! - [`FuseInitIn`] / [`FuseInitOut`] — protocol negotiation (op = `Init`)
//! - [`FuseAttr`] — file attributes shared by `Getattr`, `Setattr`, and
//!   the bodies of [`FuseEntryOut`]
//! - [`FuseEntryOut`] — directory entry returned by `Lookup`, `Mknod`,
//!   `Mkdir`, `Symlink`, `Link`
//!
//! Bodies for `Open` / `Read` / `Write` / `Readdir` come in a follow-up
//! PR — keeping this one tight on the smallest surface that lets the
//! dispatcher already negotiate a session and answer attribute queries.
//!
//! All structs follow the same convention used by [`super::FuseInHeader`]
//! / [`super::FuseOutHeader`]: explicit little-endian
//! `from_bytes` / `write_to` / `to_bytes`, no `#[repr(C)]` casting,
//! `from_bytes` accepts `>= N` so a caller can pass a buffer that also
//! contains trailing data.

use crate::FuseError;

/// On-the-wire size of [`FuseInitIn`] in bytes.
pub const FUSE_INIT_IN_LEN: usize = 16;

/// On-the-wire size of [`FuseInitOut`] in bytes (the modern 64-byte
/// shape with `flags2` and 7 reserved `unused` u32s; older kernels
/// supported a shorter variant which we don't need to emit).
pub const FUSE_INIT_OUT_LEN: usize = 64;

/// On-the-wire size of [`FuseAttr`] in bytes.
pub const FUSE_ATTR_LEN: usize = 88;

/// On-the-wire size of [`FuseEntryOut`] in bytes.
pub const FUSE_ENTRY_OUT_LEN: usize = 128;

// ---------------------------------------------------------------------------
// fuse_init_in
// ---------------------------------------------------------------------------

/// Body of a `FUSE_INIT` request (guest → host). Starts the session;
/// host replies with a [`FuseInitOut`] negotiating the active feature
/// set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseInitIn {
    /// Protocol major version the guest speaks.
    pub major: u32,
    /// Protocol minor version the guest speaks.
    pub minor: u32,
    /// Maximum readahead the guest will request (bytes).
    pub max_readahead: u32,
    /// Bitfield of `FUSE_*` feature flags the guest advertises.
    pub flags: u32,
}

impl FuseInitIn {
    /// Parse the first [`FUSE_INIT_IN_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_INIT_IN_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_INIT_IN_LEN,
            });
        }
        Ok(Self {
            major: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            minor: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            max_readahead: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            flags: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
        })
    }

    /// Serialize into `buf` (must be at least [`FUSE_INIT_IN_LEN`]).
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_INIT_IN_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_INIT_IN_LEN,
            });
        }
        buf[0..4].copy_from_slice(&self.major.to_le_bytes());
        buf[4..8].copy_from_slice(&self.minor.to_le_bytes());
        buf[8..12].copy_from_slice(&self.max_readahead.to_le_bytes());
        buf[12..16].copy_from_slice(&self.flags.to_le_bytes());
        Ok(FUSE_INIT_IN_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_INIT_IN_LEN] {
        let mut out = [0u8; FUSE_INIT_IN_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseInitIn into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_init_out
// ---------------------------------------------------------------------------

/// Body of a `FUSE_INIT` response (host → guest). The server picks the
/// minimum of `(self.major, request.major)`, advertises the features it
/// supports, and the kernel uses that to drive subsequent operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseInitOut {
    /// Protocol major version the host speaks.
    pub major: u32,
    /// Protocol minor version the host speaks.
    pub minor: u32,
    /// Maximum readahead the host accepts.
    pub max_readahead: u32,
    /// Bitfield of `FUSE_*` feature flags the host accepts.
    pub flags: u32,
    /// Recommended max number of in-flight background requests.
    pub max_background: u16,
    /// Number of background requests at which the host considers the
    /// queue congested.
    pub congestion_threshold: u16,
    /// Maximum write size the host accepts in a single `FUSE_WRITE`.
    pub max_write: u32,
    /// Time granularity in nanoseconds (1 means full nanosecond fidelity).
    pub time_gran: u32,
    /// Maximum pages per request.
    pub max_pages: u16,
    /// DAX map alignment (in `1 << x` form).
    pub map_alignment: u16,
    /// High word of the feature flag bitfield.
    pub flags2: u32,
    /// Reserved — writers MUST emit zero, readers MUST ignore.
    pub unused: [u32; 7],
}

impl FuseInitOut {
    /// Convenience constructor with all `unused` words zero and a
    /// caller-friendly subset of fields. Anything left at its default
    /// matches the conservative behaviour `virtiofsd` expects from a
    /// minimal v1 server (no DAX, no max-pages, etc).
    pub fn minimal(major: u32, minor: u32, max_readahead: u32, flags: u32, max_write: u32) -> Self {
        Self {
            major,
            minor,
            max_readahead,
            flags,
            max_background: 0,
            congestion_threshold: 0,
            max_write,
            time_gran: 1,
            max_pages: 0,
            map_alignment: 0,
            flags2: 0,
            unused: [0; 7],
        }
    }

    /// Parse the first [`FUSE_INIT_OUT_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_INIT_OUT_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_INIT_OUT_LEN,
            });
        }
        let major = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let minor = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let max_readahead = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        let flags = u32::from_le_bytes(buf[12..16].try_into().unwrap());
        let max_background = u16::from_le_bytes(buf[16..18].try_into().unwrap());
        let congestion_threshold = u16::from_le_bytes(buf[18..20].try_into().unwrap());
        let max_write = u32::from_le_bytes(buf[20..24].try_into().unwrap());
        let time_gran = u32::from_le_bytes(buf[24..28].try_into().unwrap());
        let max_pages = u16::from_le_bytes(buf[28..30].try_into().unwrap());
        let map_alignment = u16::from_le_bytes(buf[30..32].try_into().unwrap());
        let flags2 = u32::from_le_bytes(buf[32..36].try_into().unwrap());
        let mut unused = [0u32; 7];
        for (i, slot) in unused.iter_mut().enumerate() {
            let off = 36 + 4 * i;
            *slot = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        }
        Ok(Self {
            major,
            minor,
            max_readahead,
            flags,
            max_background,
            congestion_threshold,
            max_write,
            time_gran,
            max_pages,
            map_alignment,
            flags2,
            unused,
        })
    }

    /// Serialize. Reserved tail (`unused`) is written verbatim from the
    /// struct; the `minimal` constructor emits zeros there.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_INIT_OUT_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_INIT_OUT_LEN,
            });
        }
        buf[0..4].copy_from_slice(&self.major.to_le_bytes());
        buf[4..8].copy_from_slice(&self.minor.to_le_bytes());
        buf[8..12].copy_from_slice(&self.max_readahead.to_le_bytes());
        buf[12..16].copy_from_slice(&self.flags.to_le_bytes());
        buf[16..18].copy_from_slice(&self.max_background.to_le_bytes());
        buf[18..20].copy_from_slice(&self.congestion_threshold.to_le_bytes());
        buf[20..24].copy_from_slice(&self.max_write.to_le_bytes());
        buf[24..28].copy_from_slice(&self.time_gran.to_le_bytes());
        buf[28..30].copy_from_slice(&self.max_pages.to_le_bytes());
        buf[30..32].copy_from_slice(&self.map_alignment.to_le_bytes());
        buf[32..36].copy_from_slice(&self.flags2.to_le_bytes());
        for (i, w) in self.unused.iter().enumerate() {
            let off = 36 + 4 * i;
            buf[off..off + 4].copy_from_slice(&w.to_le_bytes());
        }
        Ok(FUSE_INIT_OUT_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_INIT_OUT_LEN] {
        let mut out = [0u8; FUSE_INIT_OUT_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseInitOut into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_attr
// ---------------------------------------------------------------------------

/// File attributes — the FUSE-side equivalent of a `struct stat`.
/// Embedded inside [`FuseEntryOut`] and returned standalone by
/// `FUSE_GETATTR` / `FUSE_SETATTR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseAttr {
    /// Inode number.
    pub ino: u64,
    /// File size in bytes.
    pub size: u64,
    /// Number of 512-byte blocks allocated.
    pub blocks: u64,
    /// Last-access time, seconds since epoch.
    pub atime: u64,
    /// Last-modification time, seconds since epoch.
    pub mtime: u64,
    /// Last-status-change time, seconds since epoch.
    pub ctime: u64,
    /// Sub-second part of `atime`, in nanoseconds.
    pub atimensec: u32,
    /// Sub-second part of `mtime`, in nanoseconds.
    pub mtimensec: u32,
    /// Sub-second part of `ctime`, in nanoseconds.
    pub ctimensec: u32,
    /// File mode (POSIX permission bits + type).
    pub mode: u32,
    /// Hard link count.
    pub nlink: u32,
    /// Owner user id.
    pub uid: u32,
    /// Owner group id.
    pub gid: u32,
    /// Device id (for special files).
    pub rdev: u32,
    /// Filesystem block size.
    pub blksize: u32,
    /// FUSE-specific attribute flags.
    pub flags: u32,
}

impl FuseAttr {
    /// Parse the first [`FUSE_ATTR_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_ATTR_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_ATTR_LEN,
            });
        }
        Ok(Self {
            ino: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            size: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            blocks: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            atime: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
            mtime: u64::from_le_bytes(buf[32..40].try_into().unwrap()),
            ctime: u64::from_le_bytes(buf[40..48].try_into().unwrap()),
            atimensec: u32::from_le_bytes(buf[48..52].try_into().unwrap()),
            mtimensec: u32::from_le_bytes(buf[52..56].try_into().unwrap()),
            ctimensec: u32::from_le_bytes(buf[56..60].try_into().unwrap()),
            mode: u32::from_le_bytes(buf[60..64].try_into().unwrap()),
            nlink: u32::from_le_bytes(buf[64..68].try_into().unwrap()),
            uid: u32::from_le_bytes(buf[68..72].try_into().unwrap()),
            gid: u32::from_le_bytes(buf[72..76].try_into().unwrap()),
            rdev: u32::from_le_bytes(buf[76..80].try_into().unwrap()),
            blksize: u32::from_le_bytes(buf[80..84].try_into().unwrap()),
            flags: u32::from_le_bytes(buf[84..88].try_into().unwrap()),
        })
    }

    /// Serialize into `buf`.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_ATTR_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_ATTR_LEN,
            });
        }
        buf[0..8].copy_from_slice(&self.ino.to_le_bytes());
        buf[8..16].copy_from_slice(&self.size.to_le_bytes());
        buf[16..24].copy_from_slice(&self.blocks.to_le_bytes());
        buf[24..32].copy_from_slice(&self.atime.to_le_bytes());
        buf[32..40].copy_from_slice(&self.mtime.to_le_bytes());
        buf[40..48].copy_from_slice(&self.ctime.to_le_bytes());
        buf[48..52].copy_from_slice(&self.atimensec.to_le_bytes());
        buf[52..56].copy_from_slice(&self.mtimensec.to_le_bytes());
        buf[56..60].copy_from_slice(&self.ctimensec.to_le_bytes());
        buf[60..64].copy_from_slice(&self.mode.to_le_bytes());
        buf[64..68].copy_from_slice(&self.nlink.to_le_bytes());
        buf[68..72].copy_from_slice(&self.uid.to_le_bytes());
        buf[72..76].copy_from_slice(&self.gid.to_le_bytes());
        buf[76..80].copy_from_slice(&self.rdev.to_le_bytes());
        buf[80..84].copy_from_slice(&self.blksize.to_le_bytes());
        buf[84..88].copy_from_slice(&self.flags.to_le_bytes());
        Ok(FUSE_ATTR_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_ATTR_LEN] {
        let mut out = [0u8; FUSE_ATTR_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseAttr into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_entry_out
// ---------------------------------------------------------------------------

/// Response body for `Lookup`, `Mknod`, `Mkdir`, `Symlink`, `Link`. Each
/// op resolves a path to an inode and returns a `(nodeid, attrs, ttls)`
/// triple — that's exactly what this struct encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseEntryOut {
    /// Inode number the kernel should remember for this entry.
    pub nodeid: u64,
    /// Per-inode generation, distinguishes a recycled inode from the
    /// previous incarnation.
    pub generation: u64,
    /// How many seconds the kernel may cache the path → nodeid mapping.
    pub entry_valid: u64,
    /// How many seconds the kernel may cache the embedded attributes.
    pub attr_valid: u64,
    /// Sub-second part of `entry_valid`.
    pub entry_valid_nsec: u32,
    /// Sub-second part of `attr_valid`.
    pub attr_valid_nsec: u32,
    /// Embedded file attributes.
    pub attr: FuseAttr,
}

impl FuseEntryOut {
    /// Parse the first [`FUSE_ENTRY_OUT_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_ENTRY_OUT_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_ENTRY_OUT_LEN,
            });
        }
        Ok(Self {
            nodeid: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            generation: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            entry_valid: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            attr_valid: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
            entry_valid_nsec: u32::from_le_bytes(buf[32..36].try_into().unwrap()),
            attr_valid_nsec: u32::from_le_bytes(buf[36..40].try_into().unwrap()),
            attr: FuseAttr::from_bytes(&buf[40..40 + FUSE_ATTR_LEN])?,
        })
    }

    /// Serialize into `buf`.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_ENTRY_OUT_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_ENTRY_OUT_LEN,
            });
        }
        buf[0..8].copy_from_slice(&self.nodeid.to_le_bytes());
        buf[8..16].copy_from_slice(&self.generation.to_le_bytes());
        buf[16..24].copy_from_slice(&self.entry_valid.to_le_bytes());
        buf[24..32].copy_from_slice(&self.attr_valid.to_le_bytes());
        buf[32..36].copy_from_slice(&self.entry_valid_nsec.to_le_bytes());
        buf[36..40].copy_from_slice(&self.attr_valid_nsec.to_le_bytes());
        self.attr.write_to(&mut buf[40..40 + FUSE_ATTR_LEN])?;
        Ok(FUSE_ENTRY_OUT_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_ENTRY_OUT_LEN] {
        let mut out = [0u8; FUSE_ENTRY_OUT_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseEntryOut into a fixed-size buffer must succeed");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- fuse_init_in -------------------------------------------------

    #[test]
    fn init_in_length_is_16() {
        assert_eq!(FUSE_INIT_IN_LEN, 16);
    }

    #[test]
    fn init_in_roundtrips() {
        let h = FuseInitIn {
            major: 7,
            minor: 33,
            max_readahead: 1 << 17,
            flags: 0xCAFE_F00D,
        };
        assert_eq!(FuseInitIn::from_bytes(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn init_in_field_offsets_match_spec() {
        let h = FuseInitIn {
            major: 0x0101_0101,
            minor: 0x0202_0202,
            max_readahead: 0x0303_0303,
            flags: 0x0404_0404,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..4], &[0x01; 4]);
        assert_eq!(&b[4..8], &[0x02; 4]);
        assert_eq!(&b[8..12], &[0x03; 4]);
        assert_eq!(&b[12..16], &[0x04; 4]);
    }

    #[test]
    fn init_in_accepts_longer_input_and_rejects_short() {
        let h = FuseInitIn {
            major: 7,
            minor: 33,
            max_readahead: 0,
            flags: 0,
        };
        let mut packet = h.to_bytes().to_vec();
        packet.extend_from_slice(&[0xAB; 8]);
        assert_eq!(FuseInitIn::from_bytes(&packet).unwrap(), h);

        let short = [0u8; 15];
        assert!(matches!(
            FuseInitIn::from_bytes(&short),
            Err(FuseError::ShortHeader { have: 15, need: 16 })
        ));
    }

    // ---- fuse_init_out ------------------------------------------------

    #[test]
    fn init_out_length_is_64() {
        assert_eq!(FUSE_INIT_OUT_LEN, 64);
    }

    #[test]
    fn init_out_roundtrips_including_unused() {
        let h = FuseInitOut {
            major: 7,
            minor: 33,
            max_readahead: 1 << 17,
            flags: 0x1234_5678,
            max_background: 12,
            congestion_threshold: 9,
            max_write: 1 << 20,
            time_gran: 1,
            max_pages: 256,
            map_alignment: 12,
            flags2: 0xABCD_1234,
            unused: [1, 2, 3, 4, 5, 6, 7],
        };
        assert_eq!(FuseInitOut::from_bytes(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn init_out_minimal_zeroes_optional_fields() {
        let h = FuseInitOut::minimal(7, 33, 0, 0xABCD, 1 << 20);
        assert_eq!(h.max_background, 0);
        assert_eq!(h.congestion_threshold, 0);
        assert_eq!(h.max_pages, 0);
        assert_eq!(h.map_alignment, 0);
        assert_eq!(h.flags2, 0);
        assert_eq!(h.unused, [0; 7]);
        // Time granularity is "1ns" by convention so writes don't get
        // rounded.
        assert_eq!(h.time_gran, 1);
    }

    #[test]
    fn init_out_field_offsets_match_spec() {
        let h = FuseInitOut {
            major: 0x0101_0101,
            minor: 0x0202_0202,
            max_readahead: 0x0303_0303,
            flags: 0x0404_0404,
            max_background: 0x0505,
            congestion_threshold: 0x0606,
            max_write: 0x0707_0707,
            time_gran: 0x0808_0808,
            max_pages: 0x0909,
            map_alignment: 0x0A0A,
            flags2: 0x0B0B_0B0B,
            unused: [0x0C0C_0C0C; 7],
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..4], &[0x01; 4]);
        assert_eq!(&b[4..8], &[0x02; 4]);
        assert_eq!(&b[8..12], &[0x03; 4]);
        assert_eq!(&b[12..16], &[0x04; 4]);
        assert_eq!(&b[16..18], &[0x05; 2]);
        assert_eq!(&b[18..20], &[0x06; 2]);
        assert_eq!(&b[20..24], &[0x07; 4]);
        assert_eq!(&b[24..28], &[0x08; 4]);
        assert_eq!(&b[28..30], &[0x09; 2]);
        assert_eq!(&b[30..32], &[0x0A; 2]);
        assert_eq!(&b[32..36], &[0x0B; 4]);
        assert_eq!(&b[36..40], &[0x0C; 4]);
        assert_eq!(&b[60..64], &[0x0C; 4]);
    }

    // ---- fuse_attr ----------------------------------------------------

    #[test]
    fn attr_length_is_88() {
        assert_eq!(FUSE_ATTR_LEN, 88);
    }

    #[test]
    fn attr_roundtrips() {
        let a = FuseAttr {
            ino: 0x1234,
            size: 4096,
            blocks: 8,
            atime: 1_700_000_000,
            mtime: 1_700_000_001,
            ctime: 1_700_000_002,
            atimensec: 100,
            mtimensec: 200,
            ctimensec: 300,
            mode: 0o100644,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        };
        assert_eq!(FuseAttr::from_bytes(&a.to_bytes()).unwrap(), a);
    }

    #[test]
    fn attr_field_offsets_match_spec() {
        let a = FuseAttr {
            ino: 0x0101_0101_0101_0101,
            size: 0x0202_0202_0202_0202,
            blocks: 0x0303_0303_0303_0303,
            atime: 0x0404_0404_0404_0404,
            mtime: 0x0505_0505_0505_0505,
            ctime: 0x0606_0606_0606_0606,
            atimensec: 0x0707_0707,
            mtimensec: 0x0808_0808,
            ctimensec: 0x0909_0909,
            mode: 0x0A0A_0A0A,
            nlink: 0x0B0B_0B0B,
            uid: 0x0C0C_0C0C,
            gid: 0x0D0D_0D0D,
            rdev: 0x0E0E_0E0E,
            blksize: 0x0F0F_0F0F,
            flags: 0x1010_1010,
        };
        let b = a.to_bytes();
        assert_eq!(&b[0..8], &[0x01; 8]);
        assert_eq!(&b[8..16], &[0x02; 8]);
        assert_eq!(&b[16..24], &[0x03; 8]);
        assert_eq!(&b[24..32], &[0x04; 8]);
        assert_eq!(&b[32..40], &[0x05; 8]);
        assert_eq!(&b[40..48], &[0x06; 8]);
        assert_eq!(&b[48..52], &[0x07; 4]);
        assert_eq!(&b[52..56], &[0x08; 4]);
        assert_eq!(&b[56..60], &[0x09; 4]);
        assert_eq!(&b[60..64], &[0x0A; 4]);
        assert_eq!(&b[64..68], &[0x0B; 4]);
        assert_eq!(&b[68..72], &[0x0C; 4]);
        assert_eq!(&b[72..76], &[0x0D; 4]);
        assert_eq!(&b[76..80], &[0x0E; 4]);
        assert_eq!(&b[80..84], &[0x0F; 4]);
        assert_eq!(&b[84..88], &[0x10; 4]);
    }

    // ---- fuse_entry_out -----------------------------------------------

    #[test]
    fn entry_out_length_is_128() {
        assert_eq!(FUSE_ENTRY_OUT_LEN, 128);
        // Layout sanity: 40 bytes of preamble + 88 bytes of embedded
        // attr should equal the documented length.
        assert_eq!(40 + FUSE_ATTR_LEN, FUSE_ENTRY_OUT_LEN);
    }

    #[test]
    fn entry_out_roundtrips_including_embedded_attr() {
        let attr = FuseAttr {
            ino: 99,
            size: 0,
            blocks: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
            atimensec: 0,
            mtimensec: 0,
            ctimensec: 0,
            mode: 0o040755,
            nlink: 2,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        };
        let e = FuseEntryOut {
            nodeid: 99,
            generation: 1,
            entry_valid: 5,
            attr_valid: 5,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr,
        };
        assert_eq!(FuseEntryOut::from_bytes(&e.to_bytes()).unwrap(), e);
    }

    #[test]
    fn entry_out_field_offsets_match_spec() {
        let e = FuseEntryOut {
            nodeid: 0x0101_0101_0101_0101,
            generation: 0x0202_0202_0202_0202,
            entry_valid: 0x0303_0303_0303_0303,
            attr_valid: 0x0404_0404_0404_0404,
            entry_valid_nsec: 0x0505_0505,
            attr_valid_nsec: 0x0606_0606,
            attr: FuseAttr::default(),
        };
        let b = e.to_bytes();
        assert_eq!(&b[0..8], &[0x01; 8]);
        assert_eq!(&b[8..16], &[0x02; 8]);
        assert_eq!(&b[16..24], &[0x03; 8]);
        assert_eq!(&b[24..32], &[0x04; 8]);
        assert_eq!(&b[32..36], &[0x05; 4]);
        assert_eq!(&b[36..40], &[0x06; 4]);
        // The embedded fuse_attr is all zeros.
        assert_eq!(&b[40..FUSE_ENTRY_OUT_LEN], &[0u8; FUSE_ATTR_LEN]);
    }

    // ---- Common error paths -------------------------------------------

    #[test]
    fn write_to_rejects_short_output_for_each_type() {
        let mut tiny = [0u8; 4];
        let init_in = FuseInitIn {
            major: 0,
            minor: 0,
            max_readahead: 0,
            flags: 0,
        };
        assert!(matches!(
            init_in.write_to(&mut tiny),
            Err(FuseError::ShortBuffer { .. })
        ));
        let init_out = FuseInitOut::minimal(7, 33, 0, 0, 0);
        assert!(matches!(
            init_out.write_to(&mut tiny),
            Err(FuseError::ShortBuffer { .. })
        ));
        let attr = FuseAttr::default();
        assert!(matches!(
            attr.write_to(&mut tiny),
            Err(FuseError::ShortBuffer { .. })
        ));
        let entry = FuseEntryOut::default();
        assert!(matches!(
            entry.write_to(&mut tiny),
            Err(FuseError::ShortBuffer { .. })
        ));
    }
}
