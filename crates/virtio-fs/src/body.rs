//! Per-op FUSE request / response body structs.
//!
//! Scope today — the body types needed for the M3 handshake + lookup
//! + open-read-write + readdir paths:
//!
//! - [`FuseInitIn`] / [`FuseInitOut`] — protocol negotiation (op = `Init`)
//! - [`FuseAttr`] — file attributes shared by `Getattr`, `Setattr`, and
//!   the bodies of [`FuseEntryOut`]
//! - [`FuseEntryOut`] — directory entry returned by `Lookup`, `Mknod`,
//!   `Mkdir`, `Symlink`, `Link`
//! - [`FuseOpenIn`] / [`FuseOpenOut`] — `Open` / `Opendir` (request
//!   carries `open(2)` flags; response gives the guest a file handle)
//! - [`FuseReadIn`] — `Read` and `Readdir` request (response is a bare
//!   byte stream; for `Readdir` it's a sequence of `fuse_dirent`
//!   records — see [`FuseDirentWriter`] / [`FuseDirentIter`])
//! - [`FuseWriteIn`] / [`FuseWriteOut`] — `Write`
//! - [`FuseDirentHeader`] + [`FuseDirentWriter`] / [`FuseDirentIter`] —
//!   variable-length `fuse_dirent` records that make up a `Readdir`
//!   response, plus `DT_*` file-type constants in [`dt`]
//!
//! `Flush` / `Release` headers and the dispatch loop that ties it all
//! into a virtqueue come in follow-up PRs.
//!
//! All structs follow the same convention used by [`super::FuseInHeader`]
//! / [`super::FuseOutHeader`]: explicit little-endian
//! `from_bytes` / `write_to` / `to_bytes`, no `#[repr(C)]` casting,
//! `from_bytes` accepts `>= N` so a caller can pass a buffer that also
//! contains trailing data (request body in front of read/write payload,
//! header in front of body, ...).

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

// ---------------------------------------------------------------------------
// fuse_open_in / fuse_open_out
// ---------------------------------------------------------------------------

/// On-the-wire size of [`FuseOpenIn`] in bytes.
pub const FUSE_OPEN_IN_LEN: usize = 8;

/// On-the-wire size of [`FuseOpenOut`] in bytes.
pub const FUSE_OPEN_OUT_LEN: usize = 16;

/// Body of a `FUSE_OPEN` / `FUSE_OPENDIR` request. Carries the
/// `open(2)` flags plus a small bag of FUSE-specific bits; the host
/// responds with a [`FuseOpenOut`] containing the file handle the
/// guest will use for subsequent reads / writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseOpenIn {
    /// `open(2)` flags (`O_RDONLY`, `O_WRONLY`, `O_APPEND`, ...).
    pub flags: u32,
    /// FUSE-specific bits. `0` for the conservative paths we emit
    /// today.
    pub open_flags: u32,
}

impl FuseOpenIn {
    /// Parse the first [`FUSE_OPEN_IN_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_OPEN_IN_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_OPEN_IN_LEN,
            });
        }
        Ok(Self {
            flags: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            open_flags: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
        })
    }

    /// Serialize into `buf`.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_OPEN_IN_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_OPEN_IN_LEN,
            });
        }
        buf[0..4].copy_from_slice(&self.flags.to_le_bytes());
        buf[4..8].copy_from_slice(&self.open_flags.to_le_bytes());
        Ok(FUSE_OPEN_IN_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_OPEN_IN_LEN] {
        let mut out = [0u8; FUSE_OPEN_IN_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseOpenIn into a fixed-size buffer must succeed");
        out
    }
}

/// Body of a `FUSE_OPEN` / `FUSE_OPENDIR` response — gives the guest
/// the opaque file handle it should pass back in subsequent
/// [`FuseReadIn`] / [`FuseWriteIn`] requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseOpenOut {
    /// Opaque file handle; the host keeps an internal map from `fh` to
    /// the underlying file descriptor / VFS state.
    pub fh: u64,
    /// FUSE-specific response bits (`FOPEN_DIRECT_IO`,
    /// `FOPEN_KEEP_CACHE`, ...).
    pub open_flags: u32,
    /// Padding; writers MUST emit `0`, readers MUST ignore.
    pub padding: u32,
}

impl FuseOpenOut {
    /// Parse the first [`FUSE_OPEN_OUT_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_OPEN_OUT_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_OPEN_OUT_LEN,
            });
        }
        Ok(Self {
            fh: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            open_flags: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            padding: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
        })
    }

    /// Serialize into `buf`.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_OPEN_OUT_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_OPEN_OUT_LEN,
            });
        }
        buf[0..8].copy_from_slice(&self.fh.to_le_bytes());
        buf[8..12].copy_from_slice(&self.open_flags.to_le_bytes());
        buf[12..16].copy_from_slice(&self.padding.to_le_bytes());
        Ok(FUSE_OPEN_OUT_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_OPEN_OUT_LEN] {
        let mut out = [0u8; FUSE_OPEN_OUT_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseOpenOut into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_read_in
// ---------------------------------------------------------------------------

/// On-the-wire size of [`FuseReadIn`] in bytes.
pub const FUSE_READ_IN_LEN: usize = 40;

/// Body of a `FUSE_READ` request. Tells the host which file handle to
/// read from, where to start, and how many bytes the guest can accept.
/// The response is a bare byte stream of length up to `size` —
/// no header struct, just the data after the [`super::FuseOutHeader`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseReadIn {
    /// File handle returned by [`FuseOpenOut`].
    pub fh: u64,
    /// Byte offset within the file.
    pub offset: u64,
    /// Maximum bytes the guest can accept in the response.
    pub size: u32,
    /// FUSE-specific read flags (e.g. `FUSE_READ_LOCKOWNER`).
    pub read_flags: u32,
    /// POSIX lock owner if `FUSE_READ_LOCKOWNER` is set in `read_flags`.
    pub lock_owner: u64,
    /// `open(2)` flags as carried by the original [`FuseOpenIn::flags`].
    pub flags: u32,
    /// Padding; writers MUST emit `0`, readers MUST ignore.
    pub padding: u32,
}

impl FuseReadIn {
    /// Parse the first [`FUSE_READ_IN_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_READ_IN_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_READ_IN_LEN,
            });
        }
        Ok(Self {
            fh: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            offset: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            size: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
            read_flags: u32::from_le_bytes(buf[20..24].try_into().unwrap()),
            lock_owner: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
            flags: u32::from_le_bytes(buf[32..36].try_into().unwrap()),
            padding: u32::from_le_bytes(buf[36..40].try_into().unwrap()),
        })
    }

    /// Serialize into `buf`.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_READ_IN_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_READ_IN_LEN,
            });
        }
        buf[0..8].copy_from_slice(&self.fh.to_le_bytes());
        buf[8..16].copy_from_slice(&self.offset.to_le_bytes());
        buf[16..20].copy_from_slice(&self.size.to_le_bytes());
        buf[20..24].copy_from_slice(&self.read_flags.to_le_bytes());
        buf[24..32].copy_from_slice(&self.lock_owner.to_le_bytes());
        buf[32..36].copy_from_slice(&self.flags.to_le_bytes());
        buf[36..40].copy_from_slice(&self.padding.to_le_bytes());
        Ok(FUSE_READ_IN_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_READ_IN_LEN] {
        let mut out = [0u8; FUSE_READ_IN_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseReadIn into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_write_in / fuse_write_out
// ---------------------------------------------------------------------------

/// On-the-wire size of [`FuseWriteIn`] in bytes. Same shape as
/// `fuse_read_in` modulo field semantics — kept as separate constants
/// so each type's wire size is a single named source of truth.
pub const FUSE_WRITE_IN_LEN: usize = 40;

/// On-the-wire size of [`FuseWriteOut`] in bytes.
pub const FUSE_WRITE_OUT_LEN: usize = 8;

/// Body of a `FUSE_WRITE` request. The data to be written follows
/// immediately after the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseWriteIn {
    /// File handle returned by [`FuseOpenOut`].
    pub fh: u64,
    /// Byte offset within the file.
    pub offset: u64,
    /// Number of bytes of payload to follow.
    pub size: u32,
    /// FUSE-specific write flags (`FUSE_WRITE_CACHE`, `FUSE_WRITE_LOCKOWNER`).
    pub write_flags: u32,
    /// POSIX lock owner if `FUSE_WRITE_LOCKOWNER` is set.
    pub lock_owner: u64,
    /// `open(2)` flags from the original open.
    pub flags: u32,
    /// Padding; writers MUST emit `0`, readers MUST ignore.
    pub padding: u32,
}

impl FuseWriteIn {
    /// Parse the first [`FUSE_WRITE_IN_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_WRITE_IN_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_WRITE_IN_LEN,
            });
        }
        Ok(Self {
            fh: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            offset: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            size: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
            write_flags: u32::from_le_bytes(buf[20..24].try_into().unwrap()),
            lock_owner: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
            flags: u32::from_le_bytes(buf[32..36].try_into().unwrap()),
            padding: u32::from_le_bytes(buf[36..40].try_into().unwrap()),
        })
    }

    /// Serialize into `buf`.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_WRITE_IN_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_WRITE_IN_LEN,
            });
        }
        buf[0..8].copy_from_slice(&self.fh.to_le_bytes());
        buf[8..16].copy_from_slice(&self.offset.to_le_bytes());
        buf[16..20].copy_from_slice(&self.size.to_le_bytes());
        buf[20..24].copy_from_slice(&self.write_flags.to_le_bytes());
        buf[24..32].copy_from_slice(&self.lock_owner.to_le_bytes());
        buf[32..36].copy_from_slice(&self.flags.to_le_bytes());
        buf[36..40].copy_from_slice(&self.padding.to_le_bytes());
        Ok(FUSE_WRITE_IN_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_WRITE_IN_LEN] {
        let mut out = [0u8; FUSE_WRITE_IN_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseWriteIn into a fixed-size buffer must succeed");
        out
    }
}

/// Body of a `FUSE_WRITE` response — reports how many bytes the host
/// actually committed (which may be less than the requested size on
/// short writes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseWriteOut {
    /// Bytes actually written.
    pub size: u32,
    /// Padding; writers MUST emit `0`, readers MUST ignore.
    pub padding: u32,
}

impl FuseWriteOut {
    /// Parse the first [`FUSE_WRITE_OUT_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_WRITE_OUT_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_WRITE_OUT_LEN,
            });
        }
        Ok(Self {
            size: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            padding: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
        })
    }

    /// Serialize into `buf`.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_WRITE_OUT_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_WRITE_OUT_LEN,
            });
        }
        buf[0..4].copy_from_slice(&self.size.to_le_bytes());
        buf[4..8].copy_from_slice(&self.padding.to_le_bytes());
        Ok(FUSE_WRITE_OUT_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_WRITE_OUT_LEN] {
        let mut out = [0u8; FUSE_WRITE_OUT_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseWriteOut into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_flush_in / fuse_release_in
// ---------------------------------------------------------------------------

/// On-the-wire size of [`FuseFlushIn`] in bytes.
pub const FUSE_FLUSH_IN_LEN: usize = 24;

/// On-the-wire size of [`FuseReleaseIn`] in bytes.
pub const FUSE_RELEASE_IN_LEN: usize = 24;

/// Body of a `FUSE_FLUSH` request. Sent by the kernel before a final
/// `Release` (or after every `close(2)` when the file was opened with
/// `O_SYNC`). The host responds with an empty body — the
/// [`super::FuseOutHeader::error`] field carries the status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseFlushIn {
    /// File handle returned by [`FuseOpenOut`].
    pub fh: u64,
    /// Reserved; writers MUST emit `0`, readers MUST ignore.
    pub unused: u32,
    /// Padding; writers MUST emit `0`, readers MUST ignore.
    pub padding: u32,
    /// POSIX lock owner — `0` when the file was not opened by a
    /// process that holds posix locks.
    pub lock_owner: u64,
}

impl FuseFlushIn {
    /// Parse the first [`FUSE_FLUSH_IN_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_FLUSH_IN_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_FLUSH_IN_LEN,
            });
        }
        Ok(Self {
            fh: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            unused: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            padding: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
            lock_owner: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
        })
    }

    /// Serialize into `buf`.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_FLUSH_IN_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_FLUSH_IN_LEN,
            });
        }
        buf[0..8].copy_from_slice(&self.fh.to_le_bytes());
        buf[8..12].copy_from_slice(&self.unused.to_le_bytes());
        buf[12..16].copy_from_slice(&self.padding.to_le_bytes());
        buf[16..24].copy_from_slice(&self.lock_owner.to_le_bytes());
        Ok(FUSE_FLUSH_IN_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_FLUSH_IN_LEN] {
        let mut out = [0u8; FUSE_FLUSH_IN_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseFlushIn into a fixed-size buffer must succeed");
        out
    }
}

/// Body of a `FUSE_RELEASE` / `FUSE_RELEASEDIR` request. Tells the
/// host the kernel is done with a file handle and the host can drop
/// the underlying state. Like `Flush`, the response is an empty body
/// (status in the [`super::FuseOutHeader::error`] field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseReleaseIn {
    /// File handle returned by [`FuseOpenOut`].
    pub fh: u64,
    /// `open(2)` flags from the original [`FuseOpenIn::flags`] —
    /// echoed so the host can apply the same `O_SYNC` / `O_DSYNC`
    /// semantics on close.
    pub flags: u32,
    /// FUSE-specific release bits (e.g. `FUSE_RELEASE_FLUSH`,
    /// `FUSE_RELEASE_FLOCK_UNLOCK`).
    pub release_flags: u32,
    /// POSIX lock owner — relevant when `release_flags` requests
    /// implicit `flock(2)` cleanup.
    pub lock_owner: u64,
}

impl FuseReleaseIn {
    /// Parse the first [`FUSE_RELEASE_IN_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_RELEASE_IN_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_RELEASE_IN_LEN,
            });
        }
        Ok(Self {
            fh: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            flags: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            release_flags: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
            lock_owner: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
        })
    }

    /// Serialize into `buf`.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_RELEASE_IN_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_RELEASE_IN_LEN,
            });
        }
        buf[0..8].copy_from_slice(&self.fh.to_le_bytes());
        buf[8..12].copy_from_slice(&self.flags.to_le_bytes());
        buf[12..16].copy_from_slice(&self.release_flags.to_le_bytes());
        buf[16..24].copy_from_slice(&self.lock_owner.to_le_bytes());
        Ok(FUSE_RELEASE_IN_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_RELEASE_IN_LEN] {
        let mut out = [0u8; FUSE_RELEASE_IN_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseReleaseIn into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_forget_in (FUSE_FORGET)
// ---------------------------------------------------------------------------

/// On-the-wire size of [`FuseForgetIn`] in bytes.
pub const FUSE_FORGET_IN_LEN: usize = 8;

/// Body of a `FUSE_FORGET` request. Tells the host that the kernel is
/// releasing `nlookup` references to the inode identified by the header's
/// `nodeid`. Unlike every other FUSE operation, `FUSE_FORGET` has **no
/// reply** — the host must never send a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseForgetIn {
    /// Number of lookup references the kernel is returning. The host
    /// decrements its reference count by this amount, and may free
    /// inode state once it reaches zero.
    pub nlookup: u64,
}

impl FuseForgetIn {
    /// Parse the first [`FUSE_FORGET_IN_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_FORGET_IN_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_FORGET_IN_LEN,
            });
        }
        Ok(Self {
            nlookup: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
        })
    }

    /// Serialize into `buf` (must be at least [`FUSE_FORGET_IN_LEN`] bytes).
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_FORGET_IN_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_FORGET_IN_LEN,
            });
        }
        buf[0..8].copy_from_slice(&self.nlookup.to_le_bytes());
        Ok(FUSE_FORGET_IN_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_FORGET_IN_LEN] {
        let mut out = [0u8; FUSE_FORGET_IN_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseForgetIn into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_getattr_in (FUSE_GETATTR)
// ---------------------------------------------------------------------------

/// On-the-wire size of [`FuseGetattrIn`] in bytes.
pub const FUSE_GETATTR_IN_LEN: usize = 16;

/// When set in [`FuseGetattrIn::getattr_flags`], the request targets an open
/// file handle: the host should use `fh` instead of `nodeid` to look up the
/// file.
pub const FUSE_GETATTR_FH: u32 = 1 << 0;

/// Body of a `FUSE_GETATTR` request.
///
/// The host responds with a [`FuseAttrOut`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseGetattrIn {
    /// Bitfield of `FUSE_GETATTR_*` flags (currently only
    /// [`FUSE_GETATTR_FH`]).
    pub getattr_flags: u32,
    /// Reserved; writers MUST emit `0`, readers MUST ignore.
    pub dummy: u32,
    /// Open file handle — valid only when [`FUSE_GETATTR_FH`] is set in
    /// `getattr_flags`.
    pub fh: u64,
}

impl FuseGetattrIn {
    /// Parse the first [`FUSE_GETATTR_IN_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_GETATTR_IN_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_GETATTR_IN_LEN,
            });
        }
        Ok(Self {
            getattr_flags: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            dummy: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            fh: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
        })
    }

    /// Serialize into `buf` (must be at least [`FUSE_GETATTR_IN_LEN`] bytes).
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_GETATTR_IN_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_GETATTR_IN_LEN,
            });
        }
        buf[0..4].copy_from_slice(&self.getattr_flags.to_le_bytes());
        buf[4..8].copy_from_slice(&self.dummy.to_le_bytes());
        buf[8..16].copy_from_slice(&self.fh.to_le_bytes());
        Ok(FUSE_GETATTR_IN_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_GETATTR_IN_LEN] {
        let mut out = [0u8; FUSE_GETATTR_IN_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseGetattrIn into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_attr_out (FUSE_GETATTR / FUSE_SETATTR response)
// ---------------------------------------------------------------------------

/// On-the-wire size of [`FuseAttrOut`] in bytes:
/// `attr_valid(8) + attr_valid_nsec(4) + dummy(4) + attr(88)` = 104.
pub const FUSE_ATTR_OUT_LEN: usize = 16 + FUSE_ATTR_LEN;

/// Response body for `FUSE_GETATTR` and `FUSE_SETATTR`. Contains the current
/// (or updated) file attributes together with TTL hints that tell the kernel
/// how long to cache them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseAttrOut {
    /// How long the kernel may cache the embedded attributes (seconds).
    pub attr_valid: u64,
    /// Sub-second part of `attr_valid` (nanoseconds).
    pub attr_valid_nsec: u32,
    /// Reserved; writers MUST emit `0`, readers MUST ignore.
    pub dummy: u32,
    /// Current file attributes.
    pub attr: FuseAttr,
}

impl FuseAttrOut {
    /// Parse the first [`FUSE_ATTR_OUT_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_ATTR_OUT_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_ATTR_OUT_LEN,
            });
        }
        Ok(Self {
            attr_valid: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            attr_valid_nsec: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            dummy: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
            attr: FuseAttr::from_bytes(&buf[16..16 + FUSE_ATTR_LEN])?,
        })
    }

    /// Serialize into `buf` (must be at least [`FUSE_ATTR_OUT_LEN`] bytes).
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_ATTR_OUT_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_ATTR_OUT_LEN,
            });
        }
        buf[0..8].copy_from_slice(&self.attr_valid.to_le_bytes());
        buf[8..12].copy_from_slice(&self.attr_valid_nsec.to_le_bytes());
        buf[12..16].copy_from_slice(&self.dummy.to_le_bytes());
        self.attr.write_to(&mut buf[16..16 + FUSE_ATTR_LEN])?;
        Ok(FUSE_ATTR_OUT_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_ATTR_OUT_LEN] {
        let mut out = [0u8; FUSE_ATTR_OUT_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseAttrOut into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_setattr_in (FUSE_SETATTR)
// ---------------------------------------------------------------------------

/// On-the-wire size of [`FuseSetattrIn`] in bytes.
pub const FUSE_SETATTR_IN_LEN: usize = 88;

/// `FATTR_*` valid-bit constants for the [`FuseSetattrIn::valid`] field.
///
/// Each bit tells the host which fields in [`FuseSetattrIn`] the kernel
/// wants updated. Bits not set MUST be ignored by the host.
pub mod fattr {
    /// Update the file's mode (permission bits + file type).
    pub const MODE: u32 = 1 << 0;
    /// Update the file's owner user id.
    pub const UID: u32 = 1 << 1;
    /// Update the file's owner group id.
    pub const GID: u32 = 1 << 2;
    /// Truncate / extend the file to [`super::FuseSetattrIn::size`].
    pub const SIZE: u32 = 1 << 3;
    /// Set `atime` to the value in [`super::FuseSetattrIn::atime`].
    pub const ATIME: u32 = 1 << 4;
    /// Set `mtime` to the value in [`super::FuseSetattrIn::mtime`].
    pub const MTIME: u32 = 1 << 5;
    /// The request targets an open file handle; the host should use
    /// [`super::FuseSetattrIn::fh`] for the underlying syscall.
    pub const FH: u32 = 1 << 6;
    /// Set `atime` to the current wall-clock time (ignore the `atime`
    /// field in [`super::FuseSetattrIn`]).
    pub const ATIME_NOW: u32 = 1 << 7;
    /// Set `mtime` to the current wall-clock time (ignore the `mtime`
    /// field in [`super::FuseSetattrIn`]).
    pub const MTIME_NOW: u32 = 1 << 8;
    /// Update the POSIX lock owner associated with the file.
    pub const LOCKOWNER: u32 = 1 << 9;
    /// Set `ctime` to the value in [`super::FuseSetattrIn::ctime`].
    pub const CTIME: u32 = 1 << 10;
    /// Strip set-uid / set-gid bits (requires kernel support for this flag).
    pub const KILL_SUIDGID: u32 = 1 << 11;
}

/// Body of a `FUSE_SETATTR` request. The `valid` bitmask (using [`fattr`]
/// constants) tells the host which fields to apply; all others MUST be
/// ignored.
///
/// The host responds with a [`FuseAttrOut`] containing the updated
/// attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseSetattrIn {
    /// Bitmask of `FATTR_*` bits indicating which fields are valid.
    pub valid: u32,
    /// Padding; writers MUST emit `0`, readers MUST ignore.
    pub padding: u32,
    /// Open file handle (used when [`fattr::FH`] is set in `valid`).
    pub fh: u64,
    /// New file size in bytes (used when [`fattr::SIZE`] is set).
    pub size: u64,
    /// POSIX lock owner (used when [`fattr::LOCKOWNER`] is set).
    pub lock_owner: u64,
    /// New access time, seconds since epoch (used when [`fattr::ATIME`] is set).
    pub atime: u64,
    /// New modification time, seconds since epoch (used when [`fattr::MTIME`] is set).
    pub mtime: u64,
    /// New status-change time, seconds since epoch (used when [`fattr::CTIME`] is set).
    pub ctime: u64,
    /// Sub-second part of `atime` in nanoseconds.
    pub atimensec: u32,
    /// Sub-second part of `mtime` in nanoseconds.
    pub mtimensec: u32,
    /// Sub-second part of `ctime` in nanoseconds.
    pub ctimensec: u32,
    /// New file mode (used when [`fattr::MODE`] is set).
    pub mode: u32,
    /// Reserved; writers MUST emit `0`, readers MUST ignore.
    pub unused4: u32,
    /// New owner user id (used when [`fattr::UID`] is set).
    pub uid: u32,
    /// New owner group id (used when [`fattr::GID`] is set).
    pub gid: u32,
    /// Reserved; writers MUST emit `0`, readers MUST ignore.
    pub unused5: u32,
}

impl FuseSetattrIn {
    /// Parse the first [`FUSE_SETATTR_IN_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_SETATTR_IN_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_SETATTR_IN_LEN,
            });
        }
        Ok(Self {
            valid: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            padding: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            fh: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            size: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            lock_owner: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
            atime: u64::from_le_bytes(buf[32..40].try_into().unwrap()),
            mtime: u64::from_le_bytes(buf[40..48].try_into().unwrap()),
            ctime: u64::from_le_bytes(buf[48..56].try_into().unwrap()),
            atimensec: u32::from_le_bytes(buf[56..60].try_into().unwrap()),
            mtimensec: u32::from_le_bytes(buf[60..64].try_into().unwrap()),
            ctimensec: u32::from_le_bytes(buf[64..68].try_into().unwrap()),
            mode: u32::from_le_bytes(buf[68..72].try_into().unwrap()),
            unused4: u32::from_le_bytes(buf[72..76].try_into().unwrap()),
            uid: u32::from_le_bytes(buf[76..80].try_into().unwrap()),
            gid: u32::from_le_bytes(buf[80..84].try_into().unwrap()),
            unused5: u32::from_le_bytes(buf[84..88].try_into().unwrap()),
        })
    }

    /// Serialize into `buf` (must be at least [`FUSE_SETATTR_IN_LEN`] bytes).
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_SETATTR_IN_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_SETATTR_IN_LEN,
            });
        }
        buf[0..4].copy_from_slice(&self.valid.to_le_bytes());
        buf[4..8].copy_from_slice(&self.padding.to_le_bytes());
        buf[8..16].copy_from_slice(&self.fh.to_le_bytes());
        buf[16..24].copy_from_slice(&self.size.to_le_bytes());
        buf[24..32].copy_from_slice(&self.lock_owner.to_le_bytes());
        buf[32..40].copy_from_slice(&self.atime.to_le_bytes());
        buf[40..48].copy_from_slice(&self.mtime.to_le_bytes());
        buf[48..56].copy_from_slice(&self.ctime.to_le_bytes());
        buf[56..60].copy_from_slice(&self.atimensec.to_le_bytes());
        buf[60..64].copy_from_slice(&self.mtimensec.to_le_bytes());
        buf[64..68].copy_from_slice(&self.ctimensec.to_le_bytes());
        buf[68..72].copy_from_slice(&self.mode.to_le_bytes());
        buf[72..76].copy_from_slice(&self.unused4.to_le_bytes());
        buf[76..80].copy_from_slice(&self.uid.to_le_bytes());
        buf[80..84].copy_from_slice(&self.gid.to_le_bytes());
        buf[84..88].copy_from_slice(&self.unused5.to_le_bytes());
        Ok(FUSE_SETATTR_IN_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_SETATTR_IN_LEN] {
        let mut out = [0u8; FUSE_SETATTR_IN_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseSetattrIn into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_statfs_out (FUSE_STATFS)
// ---------------------------------------------------------------------------

/// On-the-wire size of [`FuseStatfsOut`] in bytes.
pub const FUSE_STATFS_OUT_LEN: usize = 80;

/// Response body for `FUSE_STATFS`. Contains the filesystem statistics the
/// kernel's `statfs(2)` / `statvfs(2)` calls will surface to userspace.
///
/// ```c
/// struct fuse_kstatfs {
///     uint64_t blocks;      // Total data blocks (in block-size units)
///     uint64_t bfree;       // Free blocks
///     uint64_t bavail;      // Free blocks for unprivileged users
///     uint64_t files;       // Total file nodes (inodes)
///     uint64_t ffree;       // Free file nodes
///     uint32_t bsize;       // Filesystem block size
///     uint32_t namelen;     // Maximum filename length
///     uint32_t frsize;      // Fragment size (same as bsize for most FS)
///     uint32_t padding;
///     uint32_t spare[6];
/// };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseStatfsOut {
    /// Total data blocks in the filesystem (in `bsize`-byte units).
    pub blocks: u64,
    /// Free data blocks.
    pub bfree: u64,
    /// Free data blocks available to non-privileged processes.
    pub bavail: u64,
    /// Total number of file nodes (inodes).
    pub files: u64,
    /// Free file nodes.
    pub ffree: u64,
    /// Filesystem block size in bytes.
    pub bsize: u32,
    /// Maximum length of a filename component (bytes).
    pub namelen: u32,
    /// Fragment size — equal to `bsize` for most filesystems.
    pub frsize: u32,
    /// Padding; writers MUST emit `0`, readers MUST ignore.
    pub padding: u32,
    /// Reserved; writers MUST emit `0`, readers MUST ignore.
    pub spare: [u32; 6],
}

impl FuseStatfsOut {
    /// Parse the first [`FUSE_STATFS_OUT_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_STATFS_OUT_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_STATFS_OUT_LEN,
            });
        }
        let mut spare = [0u32; 6];
        for (i, slot) in spare.iter_mut().enumerate() {
            let off = 56 + 4 * i;
            *slot = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        }
        Ok(Self {
            blocks: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            bfree: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            bavail: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            files: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
            ffree: u64::from_le_bytes(buf[32..40].try_into().unwrap()),
            bsize: u32::from_le_bytes(buf[40..44].try_into().unwrap()),
            namelen: u32::from_le_bytes(buf[44..48].try_into().unwrap()),
            frsize: u32::from_le_bytes(buf[48..52].try_into().unwrap()),
            padding: u32::from_le_bytes(buf[52..56].try_into().unwrap()),
            spare,
        })
    }

    /// Serialize into `buf` (must be at least [`FUSE_STATFS_OUT_LEN`] bytes).
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_STATFS_OUT_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_STATFS_OUT_LEN,
            });
        }
        buf[0..8].copy_from_slice(&self.blocks.to_le_bytes());
        buf[8..16].copy_from_slice(&self.bfree.to_le_bytes());
        buf[16..24].copy_from_slice(&self.bavail.to_le_bytes());
        buf[24..32].copy_from_slice(&self.files.to_le_bytes());
        buf[32..40].copy_from_slice(&self.ffree.to_le_bytes());
        buf[40..44].copy_from_slice(&self.bsize.to_le_bytes());
        buf[44..48].copy_from_slice(&self.namelen.to_le_bytes());
        buf[48..52].copy_from_slice(&self.frsize.to_le_bytes());
        buf[52..56].copy_from_slice(&self.padding.to_le_bytes());
        for (i, w) in self.spare.iter().enumerate() {
            let off = 56 + 4 * i;
            buf[off..off + 4].copy_from_slice(&w.to_le_bytes());
        }
        Ok(FUSE_STATFS_OUT_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_STATFS_OUT_LEN] {
        let mut out = [0u8; FUSE_STATFS_OUT_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseStatfsOut into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_mknod_in (FUSE_MKNOD)
// ---------------------------------------------------------------------------

/// On-the-wire size of [`FuseMknodIn`] in bytes.
pub const FUSE_MKNOD_IN_LEN: usize = 16;

/// Body of a `FUSE_MKNOD` request. Creates a special file (device node,
/// FIFO, or socket). The NUL-terminated file name follows immediately after
/// this fixed header in the FUSE request buffer.
///
/// The host responds with a [`FuseEntryOut`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseMknodIn {
    /// File mode including the special file type bits and permission bits.
    pub mode: u32,
    /// Device number for `S_IFBLK` and `S_IFCHR` nodes; `0` otherwise.
    pub rdev: u32,
    /// `umask` of the creating process, for the host to apply if needed.
    pub umask: u32,
    /// Padding; writers MUST emit `0`, readers MUST ignore.
    pub padding: u32,
}

impl FuseMknodIn {
    /// Parse the first [`FUSE_MKNOD_IN_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_MKNOD_IN_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_MKNOD_IN_LEN,
            });
        }
        Ok(Self {
            mode: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            rdev: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            umask: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            padding: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
        })
    }

    /// Serialize into `buf` (must be at least [`FUSE_MKNOD_IN_LEN`] bytes).
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_MKNOD_IN_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_MKNOD_IN_LEN,
            });
        }
        buf[0..4].copy_from_slice(&self.mode.to_le_bytes());
        buf[4..8].copy_from_slice(&self.rdev.to_le_bytes());
        buf[8..12].copy_from_slice(&self.umask.to_le_bytes());
        buf[12..16].copy_from_slice(&self.padding.to_le_bytes());
        Ok(FUSE_MKNOD_IN_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_MKNOD_IN_LEN] {
        let mut out = [0u8; FUSE_MKNOD_IN_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseMknodIn into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_mkdir_in (FUSE_MKDIR)
// ---------------------------------------------------------------------------

/// On-the-wire size of [`FuseMkdirIn`] in bytes.
pub const FUSE_MKDIR_IN_LEN: usize = 8;

/// Body of a `FUSE_MKDIR` request. Creates a new directory. The NUL-
/// terminated name follows immediately after this fixed header in the
/// FUSE request buffer.
///
/// The host responds with a [`FuseEntryOut`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseMkdirIn {
    /// Directory mode bits (including `S_IFDIR`).
    pub mode: u32,
    /// `umask` of the creating process.
    pub umask: u32,
}

impl FuseMkdirIn {
    /// Parse the first [`FUSE_MKDIR_IN_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_MKDIR_IN_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_MKDIR_IN_LEN,
            });
        }
        Ok(Self {
            mode: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            umask: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
        })
    }

    /// Serialize into `buf` (must be at least [`FUSE_MKDIR_IN_LEN`] bytes).
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_MKDIR_IN_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_MKDIR_IN_LEN,
            });
        }
        buf[0..4].copy_from_slice(&self.mode.to_le_bytes());
        buf[4..8].copy_from_slice(&self.umask.to_le_bytes());
        Ok(FUSE_MKDIR_IN_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_MKDIR_IN_LEN] {
        let mut out = [0u8; FUSE_MKDIR_IN_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseMkdirIn into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_fsync_in (FUSE_FSYNC / FUSE_FSYNCDIR)
// ---------------------------------------------------------------------------

/// On-the-wire size of [`FuseFsyncIn`] in bytes.
pub const FUSE_FSYNC_IN_LEN: usize = 16;

/// When set in [`FuseFsyncIn::fsync_flags`], the host should call
/// `fdatasync(2)` (flush data but not metadata) rather than `fsync(2)`.
pub const FUSE_FSYNC_FDATASYNC: u32 = 1 << 0;

/// Body of a `FUSE_FSYNC` / `FUSE_FSYNCDIR` request.
///
/// The host responds with an empty body (status in [`super::FuseOutHeader`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseFsyncIn {
    /// File handle returned by the corresponding `Open` / `Opendir`.
    pub fh: u64,
    /// Bitfield: [`FUSE_FSYNC_FDATASYNC`] requests `fdatasync` semantics.
    pub fsync_flags: u32,
    /// Padding; writers MUST emit `0`, readers MUST ignore.
    pub padding: u32,
}

impl FuseFsyncIn {
    /// Parse the first [`FUSE_FSYNC_IN_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_FSYNC_IN_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_FSYNC_IN_LEN,
            });
        }
        Ok(Self {
            fh: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            fsync_flags: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            padding: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
        })
    }

    /// Serialize into `buf` (must be at least [`FUSE_FSYNC_IN_LEN`] bytes).
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_FSYNC_IN_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_FSYNC_IN_LEN,
            });
        }
        buf[0..8].copy_from_slice(&self.fh.to_le_bytes());
        buf[8..12].copy_from_slice(&self.fsync_flags.to_le_bytes());
        buf[12..16].copy_from_slice(&self.padding.to_le_bytes());
        Ok(FUSE_FSYNC_IN_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_FSYNC_IN_LEN] {
        let mut out = [0u8; FUSE_FSYNC_IN_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseFsyncIn into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_rename_in (FUSE_RENAME)
// ---------------------------------------------------------------------------

/// On-the-wire size of [`FuseRenameIn`] in bytes.
pub const FUSE_RENAME_IN_LEN: usize = 8;

/// Body of a `FUSE_RENAME` request.
///
/// The old entry lives under `nodeid` (from [`super::FuseInHeader`]) with
/// the NUL-terminated old name following this header. The new entry will
/// live under `newdir` with the NUL-terminated new name immediately after
/// the old name in the same request buffer.
///
/// The host responds with an empty body (status in [`super::FuseOutHeader`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseRenameIn {
    /// Inode of the destination directory.
    pub newdir: u64,
}

impl FuseRenameIn {
    /// Parse the first [`FUSE_RENAME_IN_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_RENAME_IN_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_RENAME_IN_LEN,
            });
        }
        Ok(Self {
            newdir: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
        })
    }

    /// Serialize into `buf` (must be at least [`FUSE_RENAME_IN_LEN`] bytes).
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_RENAME_IN_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_RENAME_IN_LEN,
            });
        }
        buf[0..8].copy_from_slice(&self.newdir.to_le_bytes());
        Ok(FUSE_RENAME_IN_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_RENAME_IN_LEN] {
        let mut out = [0u8; FUSE_RENAME_IN_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseRenameIn into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fuse_link_in (FUSE_LINK)
// ---------------------------------------------------------------------------

/// On-the-wire size of [`FuseLinkIn`] in bytes.
pub const FUSE_LINK_IN_LEN: usize = 8;

/// Body of a `FUSE_LINK` request. Creates a hard link under a new name.
///
/// `oldnodeid` is the inode being linked; the destination directory is
/// `nodeid` (from [`super::FuseInHeader`]); and the NUL-terminated new
/// name follows this fixed header in the FUSE request buffer.
///
/// The host responds with a [`FuseEntryOut`] for the new directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseLinkIn {
    /// Inode of the existing file to link.
    pub oldnodeid: u64,
}

impl FuseLinkIn {
    /// Parse the first [`FUSE_LINK_IN_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_LINK_IN_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_LINK_IN_LEN,
            });
        }
        Ok(Self {
            oldnodeid: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
        })
    }

    /// Serialize into `buf` (must be at least [`FUSE_LINK_IN_LEN`] bytes).
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_LINK_IN_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_LINK_IN_LEN,
            });
        }
        buf[0..8].copy_from_slice(&self.oldnodeid.to_le_bytes());
        Ok(FUSE_LINK_IN_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_LINK_IN_LEN] {
        let mut out = [0u8; FUSE_LINK_IN_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseLinkIn into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// fopen flags (FUSE_OPEN / FUSE_OPENDIR response)
// ---------------------------------------------------------------------------

/// `FOPEN_*` flag bits carried in [`FuseOpenOut::open_flags`].
///
/// Set by the host in the `FUSE_OPEN` response to tell the kernel how to
/// handle I/O for this file handle.
pub mod fopen {
    /// Bypass the page cache for this file (direct I/O). Every read/write
    /// goes straight to the host without kernel buffering.
    pub const DIRECT_IO: u32 = 1 << 0;
    /// Do not invalidate the kernel's data cache when this file is opened.
    /// Useful when the host can guarantee cache coherence externally.
    pub const KEEP_CACHE: u32 = 1 << 1;
    /// The file is not seekable; the kernel will reject `lseek(2)`.
    pub const NONSEEKABLE: u32 = 1 << 2;
    /// Cache `FUSE_READDIR` responses in the kernel's dentry cache.
    pub const CACHE_DIR: u32 = 1 << 3;
    /// The host uses stream-style I/O (no random access).
    pub const STREAM: u32 = 1 << 4;
    /// Do not send `FUSE_FLUSH` on `close(2)` for this handle.
    pub const NOFLUSH: u32 = 1 << 5;
}

// ---------------------------------------------------------------------------
// fuse_dirent (FUSE_READDIR)
// ---------------------------------------------------------------------------

/// On-the-wire size of [`FuseDirentHeader`] in bytes — the fixed portion
/// preceding the variable-length name.
pub const FUSE_DIRENT_HDR_LEN: usize = 24;

/// Round-up alignment for `fuse_dirent` records on the wire. Each record
/// (header + name) is padded to a multiple of this so the next record
/// starts on an 8-byte boundary.
pub const FUSE_DIRENT_ALIGN: usize = 8;

/// `DT_*` file type constants matching `<dirent.h>`. Carried in
/// [`FuseDirentHeader::ftype`] and pinned by [`dt`] tests.
pub mod dt {
    /// Unknown file type. Servers with no idea what type a name refers
    /// to should send `Unknown`; the kernel will issue a follow-up
    /// `Lookup` to find out.
    pub const UNKNOWN: u32 = 0;
    /// FIFO (named pipe).
    pub const FIFO: u32 = 1;
    /// Character device.
    pub const CHR: u32 = 2;
    /// Directory.
    pub const DIR: u32 = 4;
    /// Block device.
    pub const BLK: u32 = 6;
    /// Regular file.
    pub const REG: u32 = 8;
    /// Symbolic link.
    pub const LNK: u32 = 10;
    /// UNIX-domain socket.
    pub const SOCK: u32 = 12;
}

/// Round `n` up to the next multiple of [`FUSE_DIRENT_ALIGN`].
#[inline]
pub fn fuse_dirent_padded_size(name_len: usize) -> usize {
    let total = FUSE_DIRENT_HDR_LEN + name_len;
    (total + FUSE_DIRENT_ALIGN - 1) & !(FUSE_DIRENT_ALIGN - 1)
}

/// Fixed 24-byte header preceding each `fuse_dirent` record. The name
/// follows immediately and is `namelen` bytes (no NUL terminator);
/// the record is then padded to a multiple of [`FUSE_DIRENT_ALIGN`]
/// so the next record starts aligned.
///
/// ```c
/// struct fuse_dirent {
///     uint64_t ino;
///     uint64_t off;
///     uint32_t namelen;
///     uint32_t type;
///     char     name[];   // namelen bytes, then 0..7 padding
/// };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseDirentHeader {
    /// Inode number.
    pub ino: u64,
    /// Position to seek to in order to read the *next* record. The
    /// kernel echoes this in subsequent [`FuseReadIn::offset`] requests.
    pub off: u64,
    /// Length of the name that follows (bytes; no NUL terminator).
    pub namelen: u32,
    /// `DT_*` file type — see the [`dt`] module.
    pub ftype: u32,
}

impl FuseDirentHeader {
    /// Parse the first [`FUSE_DIRENT_HDR_LEN`] bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FuseError> {
        if buf.len() < FUSE_DIRENT_HDR_LEN {
            return Err(FuseError::ShortHeader {
                have: buf.len(),
                need: FUSE_DIRENT_HDR_LEN,
            });
        }
        Ok(Self {
            ino: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            off: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            namelen: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
            ftype: u32::from_le_bytes(buf[20..24].try_into().unwrap()),
        })
    }

    /// Serialize into `buf`.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, FuseError> {
        if buf.len() < FUSE_DIRENT_HDR_LEN {
            return Err(FuseError::ShortBuffer {
                have: buf.len(),
                need: FUSE_DIRENT_HDR_LEN,
            });
        }
        buf[0..8].copy_from_slice(&self.ino.to_le_bytes());
        buf[8..16].copy_from_slice(&self.off.to_le_bytes());
        buf[16..20].copy_from_slice(&self.namelen.to_le_bytes());
        buf[20..24].copy_from_slice(&self.ftype.to_le_bytes());
        Ok(FUSE_DIRENT_HDR_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; FUSE_DIRENT_HDR_LEN] {
        let mut out = [0u8; FUSE_DIRENT_HDR_LEN];
        self.write_to(&mut out)
            .expect("serializing FuseDirentHeader into a fixed-size buffer must succeed");
        out
    }
}

/// Errors returned by [`FuseDirentWriter::push`] when the entry can't
/// fit in the response buffer the host advertised. Kept distinct from
/// [`FuseError`] so the dispatcher can decide to flush + retry rather
/// than treat it as a parse failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirentWriteError {
    /// Adding this record (header + name + padding) would exceed the
    /// buffer cap the kernel asked for.
    BufferFull {
        /// Bytes the entry would consume.
        need: usize,
        /// Bytes left in the cap.
        remaining: usize,
    },
    /// Name length exceeds `u32::MAX`. Practically impossible (POSIX
    /// caps NAME_MAX at 255) but we surface rather than truncate.
    NameTooLong {
        /// Length we got handed.
        len: usize,
    },
}

impl std::fmt::Display for DirentWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BufferFull { need, remaining } => {
                write!(f, "dirent buffer full: need {need}, have {remaining} left")
            }
            Self::NameTooLong { len } => write!(f, "dirent name too long: {len} bytes"),
        }
    }
}

impl std::error::Error for DirentWriteError {}

/// Builds a `FUSE_READDIR` response payload.
///
/// The reply body for `FUSE_READDIR` is a sequence of `fuse_dirent`
/// records concatenated, each padded to [`FUSE_DIRENT_ALIGN`] bytes.
/// The kernel caps the total size via the request's `size` field; we
/// honour that cap by refusing further records once the next one
/// wouldn't fit, returning [`DirentWriteError::BufferFull`].
///
/// ```
/// # use virtio_fs::body::{dt, FuseDirentWriter};
/// let mut w = FuseDirentWriter::with_capacity(1024);
/// w.push(7, 1, dt::DIR, b".").unwrap();
/// w.push(8, 2, dt::DIR, b"..").unwrap();
/// w.push(9, 3, dt::REG, b"hello.txt").unwrap();
/// let _ = w.into_bytes();
/// ```
#[derive(Debug)]
pub struct FuseDirentWriter {
    buf: Vec<u8>,
    cap: usize,
}

impl FuseDirentWriter {
    /// Construct a writer that will refuse to grow past `cap` bytes.
    /// Use the `size` field of the originating [`FuseReadIn`] request
    /// as the cap.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            cap,
        }
    }

    /// Total bytes written so far (including padding).
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// `true` when no records have been appended.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Bytes still available before [`Self::push`] would refuse.
    pub fn remaining(&self) -> usize {
        self.cap.saturating_sub(self.buf.len())
    }

    /// Append a record. Returns [`DirentWriteError::BufferFull`] if
    /// the padded record wouldn't fit; the writer is unchanged in
    /// that case (so the caller can flush what's been written so far
    /// and start a fresh writer for the next page).
    pub fn push(
        &mut self,
        ino: u64,
        off: u64,
        ftype: u32,
        name: &[u8],
    ) -> Result<(), DirentWriteError> {
        let namelen = u32::try_from(name.len())
            .map_err(|_| DirentWriteError::NameTooLong { len: name.len() })?;
        let need = fuse_dirent_padded_size(name.len());
        if self.buf.len() + need > self.cap {
            return Err(DirentWriteError::BufferFull {
                need,
                remaining: self.remaining(),
            });
        }
        let header = FuseDirentHeader {
            ino,
            off,
            namelen,
            ftype,
        };
        self.buf.extend_from_slice(&header.to_bytes());
        self.buf.extend_from_slice(name);
        // Pad to FUSE_DIRENT_ALIGN.
        let pad = need - FUSE_DIRENT_HDR_LEN - name.len();
        for _ in 0..pad {
            self.buf.push(0);
        }
        Ok(())
    }

    /// Consume the writer and return the assembled payload.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// Iterator that walks a `FUSE_READDIR` response payload, yielding one
/// `(FuseDirentHeader, &[u8] name)` pair per record. Stops cleanly at
/// end-of-buffer; surfaces [`FuseError::ShortHeader`] if a trailing
/// truncated record is encountered.
#[derive(Debug)]
pub struct FuseDirentIter<'a> {
    rest: &'a [u8],
    failed: bool,
}

impl<'a> FuseDirentIter<'a> {
    /// Wrap a payload buffer.
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            rest: buf,
            failed: false,
        }
    }
}

impl<'a> Iterator for FuseDirentIter<'a> {
    type Item = Result<(FuseDirentHeader, &'a [u8]), FuseError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.rest.is_empty() {
            return None;
        }
        // Header
        if self.rest.len() < FUSE_DIRENT_HDR_LEN {
            self.failed = true;
            return Some(Err(FuseError::ShortHeader {
                have: self.rest.len(),
                need: FUSE_DIRENT_HDR_LEN,
            }));
        }
        let hdr = match FuseDirentHeader::from_bytes(&self.rest[..FUSE_DIRENT_HDR_LEN]) {
            Ok(h) => h,
            Err(e) => {
                self.failed = true;
                return Some(Err(e));
            }
        };
        let name_len = hdr.namelen as usize;
        let total = fuse_dirent_padded_size(name_len);
        if self.rest.len() < total {
            self.failed = true;
            return Some(Err(FuseError::ShortHeader {
                have: self.rest.len(),
                need: total,
            }));
        }
        let name = &self.rest[FUSE_DIRENT_HDR_LEN..FUSE_DIRENT_HDR_LEN + name_len];
        self.rest = &self.rest[total..];
        Some(Ok((hdr, name)))
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

    // ---- fuse_open_in / fuse_open_out ---------------------------------

    #[test]
    fn open_lengths_match_spec() {
        assert_eq!(FUSE_OPEN_IN_LEN, 8);
        assert_eq!(FUSE_OPEN_OUT_LEN, 16);
    }

    #[test]
    fn open_in_roundtrips_and_offsets_match_spec() {
        let h = FuseOpenIn {
            flags: 0x0101_0101,
            open_flags: 0x0202_0202,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..4], &[0x01; 4]);
        assert_eq!(&b[4..8], &[0x02; 4]);
        assert_eq!(FuseOpenIn::from_bytes(&b).unwrap(), h);
    }

    #[test]
    fn open_out_roundtrips_and_offsets_match_spec() {
        let h = FuseOpenOut {
            fh: 0x0101_0101_0101_0101,
            open_flags: 0x0202_0202,
            padding: 0x0303_0303,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..8], &[0x01; 8]);
        assert_eq!(&b[8..12], &[0x02; 4]);
        assert_eq!(&b[12..16], &[0x03; 4]);
        assert_eq!(FuseOpenOut::from_bytes(&b).unwrap(), h);
    }

    #[test]
    fn open_in_accepts_longer_input_and_rejects_short() {
        let h = FuseOpenIn::default();
        let mut packet = h.to_bytes().to_vec();
        packet.extend_from_slice(&[0xAB; 8]);
        assert_eq!(FuseOpenIn::from_bytes(&packet).unwrap(), h);
        let short = [0u8; 7];
        assert!(matches!(
            FuseOpenIn::from_bytes(&short),
            Err(FuseError::ShortHeader { have: 7, need: 8 })
        ));
    }

    #[test]
    fn open_out_accepts_longer_input_and_rejects_short() {
        let h = FuseOpenOut::default();
        let mut packet = h.to_bytes().to_vec();
        packet.extend_from_slice(&[0xAB; 8]);
        assert_eq!(FuseOpenOut::from_bytes(&packet).unwrap(), h);
        let short = [0u8; 15];
        assert!(matches!(
            FuseOpenOut::from_bytes(&short),
            Err(FuseError::ShortHeader { have: 15, need: 16 })
        ));
    }

    // ---- fuse_read_in -------------------------------------------------

    #[test]
    fn read_in_length_is_40() {
        assert_eq!(FUSE_READ_IN_LEN, 40);
    }

    #[test]
    fn read_in_roundtrips_and_offsets_match_spec() {
        let h = FuseReadIn {
            fh: 0x0101_0101_0101_0101,
            offset: 0x0202_0202_0202_0202,
            size: 0x0303_0303,
            read_flags: 0x0404_0404,
            lock_owner: 0x0505_0505_0505_0505,
            flags: 0x0606_0606,
            padding: 0x0707_0707,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..8], &[0x01; 8]);
        assert_eq!(&b[8..16], &[0x02; 8]);
        assert_eq!(&b[16..20], &[0x03; 4]);
        assert_eq!(&b[20..24], &[0x04; 4]);
        assert_eq!(&b[24..32], &[0x05; 8]);
        assert_eq!(&b[32..36], &[0x06; 4]);
        assert_eq!(&b[36..40], &[0x07; 4]);
        assert_eq!(FuseReadIn::from_bytes(&b).unwrap(), h);
    }

    #[test]
    fn read_in_rejects_short_input() {
        let short = [0u8; 39];
        assert!(matches!(
            FuseReadIn::from_bytes(&short),
            Err(FuseError::ShortHeader { have: 39, need: 40 })
        ));
    }

    // ---- fuse_write_in / fuse_write_out -------------------------------

    #[test]
    fn write_lengths_match_spec() {
        assert_eq!(FUSE_WRITE_IN_LEN, 40);
        assert_eq!(FUSE_WRITE_OUT_LEN, 8);
    }

    #[test]
    fn write_in_roundtrips_and_offsets_match_spec() {
        let h = FuseWriteIn {
            fh: 0x0101_0101_0101_0101,
            offset: 0x0202_0202_0202_0202,
            size: 0x0303_0303,
            write_flags: 0x0404_0404,
            lock_owner: 0x0505_0505_0505_0505,
            flags: 0x0606_0606,
            padding: 0x0707_0707,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..8], &[0x01; 8]);
        assert_eq!(&b[8..16], &[0x02; 8]);
        assert_eq!(&b[16..20], &[0x03; 4]);
        assert_eq!(&b[20..24], &[0x04; 4]);
        assert_eq!(&b[24..32], &[0x05; 8]);
        assert_eq!(&b[32..36], &[0x06; 4]);
        assert_eq!(&b[36..40], &[0x07; 4]);
        assert_eq!(FuseWriteIn::from_bytes(&b).unwrap(), h);
    }

    #[test]
    fn write_in_and_read_in_share_layout() {
        // The two are intentionally identical on the wire (modulo the
        // *_flags semantics). Pin that any byte slice that decodes as
        // one decodes as the other with the same numeric values.
        let bytes = FuseReadIn {
            fh: 0xAB,
            offset: 0x1234,
            size: 100,
            read_flags: 1,
            lock_owner: 0xC0FFEE,
            flags: 2,
            padding: 0,
        }
        .to_bytes();
        let r = FuseReadIn::from_bytes(&bytes).unwrap();
        let w = FuseWriteIn::from_bytes(&bytes).unwrap();
        assert_eq!(r.fh, w.fh);
        assert_eq!(r.offset, w.offset);
        assert_eq!(r.size, w.size);
        assert_eq!(r.read_flags, w.write_flags);
        assert_eq!(r.lock_owner, w.lock_owner);
        assert_eq!(r.flags, w.flags);
    }

    #[test]
    fn write_out_roundtrips_and_offsets_match_spec() {
        let h = FuseWriteOut {
            size: 0x0101_0101,
            padding: 0x0202_0202,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..4], &[0x01; 4]);
        assert_eq!(&b[4..8], &[0x02; 4]);
        assert_eq!(FuseWriteOut::from_bytes(&b).unwrap(), h);
    }

    #[test]
    fn io_body_write_to_rejects_short_output() {
        let mut tiny = [0u8; 3];
        assert!(matches!(
            FuseOpenIn::default().write_to(&mut tiny),
            Err(FuseError::ShortBuffer { .. })
        ));
        assert!(matches!(
            FuseOpenOut::default().write_to(&mut tiny),
            Err(FuseError::ShortBuffer { .. })
        ));
        assert!(matches!(
            FuseReadIn::default().write_to(&mut tiny),
            Err(FuseError::ShortBuffer { .. })
        ));
        assert!(matches!(
            FuseWriteIn::default().write_to(&mut tiny),
            Err(FuseError::ShortBuffer { .. })
        ));
        assert!(matches!(
            FuseWriteOut::default().write_to(&mut tiny),
            Err(FuseError::ShortBuffer { .. })
        ));
    }

    // ---- fuse_dirent --------------------------------------------------

    #[test]
    fn dirent_constants_match_spec() {
        assert_eq!(FUSE_DIRENT_HDR_LEN, 24);
        assert_eq!(FUSE_DIRENT_ALIGN, 8);
        // DT_* values are pinned by <dirent.h>.
        assert_eq!(dt::UNKNOWN, 0);
        assert_eq!(dt::FIFO, 1);
        assert_eq!(dt::CHR, 2);
        assert_eq!(dt::DIR, 4);
        assert_eq!(dt::BLK, 6);
        assert_eq!(dt::REG, 8);
        assert_eq!(dt::LNK, 10);
        assert_eq!(dt::SOCK, 12);
    }

    #[test]
    fn dirent_padded_size_rounds_up_to_eight() {
        // header alone: 24 bytes (already aligned).
        assert_eq!(fuse_dirent_padded_size(0), 24);
        // 1-byte name: 24 + 1 = 25 → round up to 32.
        assert_eq!(fuse_dirent_padded_size(1), 32);
        // 8-byte name: 24 + 8 = 32 (already aligned).
        assert_eq!(fuse_dirent_padded_size(8), 32);
        // 9-byte name: 24 + 9 = 33 → 40.
        assert_eq!(fuse_dirent_padded_size(9), 40);
        // 255-byte name (POSIX NAME_MAX): 24 + 255 = 279 → 280.
        assert_eq!(fuse_dirent_padded_size(255), 280);
    }

    #[test]
    fn dirent_header_roundtrips_and_offsets_match_spec() {
        let h = FuseDirentHeader {
            ino: 0x0101_0101_0101_0101,
            off: 0x0202_0202_0202_0202,
            namelen: 0x0303_0303,
            ftype: 0x0404_0404,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..8], &[0x01; 8]);
        assert_eq!(&b[8..16], &[0x02; 8]);
        assert_eq!(&b[16..20], &[0x03; 4]);
        assert_eq!(&b[20..24], &[0x04; 4]);
        assert_eq!(FuseDirentHeader::from_bytes(&b).unwrap(), h);
    }

    #[test]
    fn dirent_writer_then_iter_round_trip_three_entries() {
        let entries: &[(u64, u64, u32, &[u8])] = &[
            (7, 1, dt::DIR, b"."),         // 24 + 1 → 32 bytes
            (8, 2, dt::DIR, b".."),        // 24 + 2 → 32 bytes
            (9, 3, dt::REG, b"hello.txt"), // 24 + 9 → 40 bytes
        ];
        let mut w = FuseDirentWriter::with_capacity(1024);
        for (ino, off, ftype, name) in entries {
            w.push(*ino, *off, *ftype, name).expect("push");
        }
        let payload = w.into_bytes();
        // Total = 32 + 32 + 40 = 104 bytes.
        assert_eq!(payload.len(), 104);

        let read: Vec<(FuseDirentHeader, Vec<u8>)> = FuseDirentIter::new(&payload)
            .map(|r| {
                let (h, n) = r.expect("dirent");
                (h, n.to_vec())
            })
            .collect();
        assert_eq!(read.len(), 3);
        for (i, (got_hdr, got_name)) in read.iter().enumerate() {
            let (ino, off, ftype, name) = entries[i];
            assert_eq!(got_hdr.ino, ino);
            assert_eq!(got_hdr.off, off);
            assert_eq!(got_hdr.ftype, ftype);
            assert_eq!(got_hdr.namelen as usize, name.len());
            assert_eq!(got_name, name, "entry {i}");
        }
    }

    #[test]
    fn dirent_writer_pads_each_record_to_alignment() {
        let mut w = FuseDirentWriter::with_capacity(1024);
        w.push(1, 1, dt::REG, b"a").unwrap(); // 24 + 1 → 32
        w.push(2, 2, dt::REG, b"ab").unwrap(); // 24 + 2 → 32
        let bytes = w.into_bytes();
        assert_eq!(bytes.len() % FUSE_DIRENT_ALIGN, 0);
        // Tail bytes of each record (the padding) must be zero so the
        // payload is byte-deterministic.
        assert_eq!(&bytes[25..32], &[0u8; 7]); // padding after "a"
        assert_eq!(&bytes[32 + 26..32 + 32], &[0u8; 6]); // padding after "ab"
    }

    #[test]
    fn dirent_writer_refuses_when_record_would_exceed_cap() {
        // Cap = exactly 32 bytes → one entry of 24+8 fits, second one
        // doesn't.
        let mut w = FuseDirentWriter::with_capacity(32);
        w.push(1, 1, dt::REG, b"01234567").expect("first fits");
        let err = w.push(2, 2, dt::REG, b"x").expect_err("second must fail");
        assert!(matches!(
            err,
            DirentWriteError::BufferFull {
                need: 32,
                remaining: 0,
            }
        ));
        // Buffer should be unchanged after the failed push.
        assert_eq!(w.len(), 32);
    }

    #[test]
    fn dirent_iter_surfaces_short_header_on_truncated_input() {
        // Build one valid entry then truncate mid-record.
        let mut w = FuseDirentWriter::with_capacity(1024);
        w.push(1, 1, dt::REG, b"x").unwrap();
        let mut bytes = w.into_bytes();
        bytes.truncate(bytes.len() - 5); // chop into the padding/name
        let results: Vec<_> = FuseDirentIter::new(&bytes).collect();
        // First should succeed up to the namelen byte / fail because
        // the iterator computes total record size before reading the
        // name. Either way, *some* result must be Err and after the
        // Err the iterator stops.
        // With truncation eating into the padded region, the header
        // claimed 1 byte name (32 total) but only 27 are present, so
        // we expect ShortHeader { have: 27, need: 32 }.
        assert_eq!(results.len(), 1, "got: {results:?}");
        assert!(matches!(
            results[0],
            Err(FuseError::ShortHeader { have: 27, need: 32 })
        ));
    }

    #[test]
    fn dirent_iter_handles_empty_buffer_without_error() {
        let results: Vec<_> = FuseDirentIter::new(&[]).collect();
        assert!(results.is_empty());
    }

    #[test]
    fn dirent_writer_remaining_tracks_bytes_left() {
        let mut w = FuseDirentWriter::with_capacity(64);
        assert_eq!(w.remaining(), 64);
        assert!(w.is_empty());
        w.push(1, 1, dt::REG, b"x").unwrap(); // 32 bytes
        assert_eq!(w.remaining(), 32);
        assert!(!w.is_empty());
    }

    // ---- fuse_flush_in / fuse_release_in ------------------------------

    #[test]
    fn flush_release_lengths_match_spec() {
        assert_eq!(FUSE_FLUSH_IN_LEN, 24);
        assert_eq!(FUSE_RELEASE_IN_LEN, 24);
    }

    #[test]
    fn flush_in_roundtrips_and_offsets_match_spec() {
        let h = FuseFlushIn {
            fh: 0x0101_0101_0101_0101,
            unused: 0x0202_0202,
            padding: 0x0303_0303,
            lock_owner: 0x0404_0404_0404_0404,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..8], &[0x01; 8]);
        assert_eq!(&b[8..12], &[0x02; 4]);
        assert_eq!(&b[12..16], &[0x03; 4]);
        assert_eq!(&b[16..24], &[0x04; 8]);
        assert_eq!(FuseFlushIn::from_bytes(&b).unwrap(), h);
    }

    #[test]
    fn release_in_roundtrips_and_offsets_match_spec() {
        let h = FuseReleaseIn {
            fh: 0x0101_0101_0101_0101,
            flags: 0x0202_0202,
            release_flags: 0x0303_0303,
            lock_owner: 0x0404_0404_0404_0404,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..8], &[0x01; 8]);
        assert_eq!(&b[8..12], &[0x02; 4]);
        assert_eq!(&b[12..16], &[0x03; 4]);
        assert_eq!(&b[16..24], &[0x04; 8]);
        assert_eq!(FuseReleaseIn::from_bytes(&b).unwrap(), h);
    }

    #[test]
    fn flush_in_accepts_longer_input_and_rejects_short() {
        let h = FuseFlushIn::default();
        let mut packet = h.to_bytes().to_vec();
        packet.extend_from_slice(&[0xAB; 8]);
        assert_eq!(FuseFlushIn::from_bytes(&packet).unwrap(), h);
        let short = [0u8; 23];
        assert!(matches!(
            FuseFlushIn::from_bytes(&short),
            Err(FuseError::ShortHeader { have: 23, need: 24 })
        ));
    }

    #[test]
    fn release_in_accepts_longer_input_and_rejects_short() {
        let h = FuseReleaseIn::default();
        let mut packet = h.to_bytes().to_vec();
        packet.extend_from_slice(&[0xAB; 8]);
        assert_eq!(FuseReleaseIn::from_bytes(&packet).unwrap(), h);
        let short = [0u8; 23];
        assert!(matches!(
            FuseReleaseIn::from_bytes(&short),
            Err(FuseError::ShortHeader { have: 23, need: 24 })
        ));
    }

    #[test]
    fn flush_release_share_layout_for_fh_and_lock_owner() {
        // The first u64 (fh) and last u64 (lock_owner) are at the same
        // offsets in both structs — pin that, since the dispatcher may
        // peek at fh without committing to a specific op type.
        let bytes = FuseReleaseIn {
            fh: 0xDEADBEEF,
            flags: 0,
            release_flags: 0,
            lock_owner: 0xCAFEF00D,
        }
        .to_bytes();
        let f = FuseFlushIn::from_bytes(&bytes).unwrap();
        assert_eq!(f.fh, 0xDEADBEEF);
        assert_eq!(f.lock_owner, 0xCAFEF00D);
    }

    #[test]
    fn flush_release_write_to_rejects_short_output() {
        let mut tiny = [0u8; 8];
        assert!(matches!(
            FuseFlushIn::default().write_to(&mut tiny),
            Err(FuseError::ShortBuffer { have: 8, need: 24 })
        ));
        assert!(matches!(
            FuseReleaseIn::default().write_to(&mut tiny),
            Err(FuseError::ShortBuffer { have: 8, need: 24 })
        ));
    }

    // ---- FuseForgetIn -------------------------------------------------

    #[test]
    fn forget_in_length_is_8() {
        assert_eq!(FUSE_FORGET_IN_LEN, 8);
    }

    #[test]
    fn forget_in_roundtrips() {
        let h = FuseForgetIn {
            nlookup: 0xDEAD_BEEF_CAFE_F00D,
        };
        let back = FuseForgetIn::from_bytes(&h.to_bytes()).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn forget_in_field_offset_matches_spec() {
        let h = FuseForgetIn {
            nlookup: 0x0101_0101_0101_0101,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..8], &[0x01u8; 8]);
    }

    #[test]
    fn forget_in_rejects_short_input() {
        let short = [0u8; 7];
        assert!(matches!(
            FuseForgetIn::from_bytes(&short),
            Err(FuseError::ShortHeader { have: 7, need: 8 })
        ));
    }

    // ---- FuseGetattrIn -----------------------------------------------

    #[test]
    fn getattr_in_length_is_16() {
        assert_eq!(FUSE_GETATTR_IN_LEN, 16);
    }

    #[test]
    fn getattr_in_roundtrips() {
        let h = FuseGetattrIn {
            getattr_flags: FUSE_GETATTR_FH,
            dummy: 0,
            fh: 0xCAFE_1234_5678_90AB,
        };
        let back = FuseGetattrIn::from_bytes(&h.to_bytes()).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn getattr_in_field_offsets_match_spec() {
        let h = FuseGetattrIn {
            getattr_flags: 0x0101_0101,
            dummy: 0x0202_0202,
            fh: 0x0303_0303_0303_0303,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..4], &[0x01; 4]);
        assert_eq!(&b[4..8], &[0x02; 4]);
        assert_eq!(&b[8..16], &[0x03; 8]);
    }

    #[test]
    fn getattr_in_rejects_short_input() {
        let short = [0u8; 15];
        assert!(matches!(
            FuseGetattrIn::from_bytes(&short),
            Err(FuseError::ShortHeader { have: 15, need: 16 })
        ));
    }

    #[test]
    fn fuse_getattr_fh_flag_is_bit_zero() {
        assert_eq!(FUSE_GETATTR_FH, 1);
    }

    // ---- FuseAttrOut -------------------------------------------------

    #[test]
    fn attr_out_length_is_104() {
        assert_eq!(FUSE_ATTR_OUT_LEN, 104);
        assert_eq!(FUSE_ATTR_OUT_LEN, 16 + FUSE_ATTR_LEN);
    }

    #[test]
    fn attr_out_roundtrips_all_fields() {
        let h = FuseAttrOut {
            attr_valid: 5,
            attr_valid_nsec: 999_999_999,
            dummy: 0,
            attr: FuseAttr {
                ino: 42,
                size: 1024,
                ..FuseAttr::default()
            },
        };
        assert_eq!(FuseAttrOut::from_bytes(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn attr_out_field_offsets_match_spec() {
        // All-distinct pattern: attr_valid at [0..8], attr_valid_nsec at
        // [8..12], dummy at [12..16], attr starts at [16].
        let h = FuseAttrOut {
            attr_valid: 0x0101_0101_0101_0101,
            attr_valid_nsec: 0x0202_0202,
            dummy: 0x0303_0303,
            attr: FuseAttr::default(),
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..8], &[0x01; 8]);
        assert_eq!(&b[8..12], &[0x02; 4]);
        assert_eq!(&b[12..16], &[0x03; 4]);
        // FuseAttr starts at offset 16; default is all-zero.
        assert_eq!(&b[16..FUSE_ATTR_OUT_LEN], &[0u8; FUSE_ATTR_LEN]);
    }

    #[test]
    fn attr_out_rejects_short_input() {
        let short = [0u8; 103];
        assert!(matches!(
            FuseAttrOut::from_bytes(&short),
            Err(FuseError::ShortHeader {
                have: 103,
                need: 104
            })
        ));
    }

    // ---- FuseSetattrIn -----------------------------------------------

    #[test]
    fn setattr_in_length_is_88() {
        assert_eq!(FUSE_SETATTR_IN_LEN, 88);
    }

    #[test]
    fn setattr_in_roundtrips_all_fields() {
        let h = FuseSetattrIn {
            valid: fattr::MODE | fattr::UID | fattr::SIZE,
            padding: 0,
            fh: 0xABCD_1234,
            size: 4096,
            lock_owner: 0,
            atime: 1_700_000_000,
            mtime: 1_700_000_001,
            ctime: 1_700_000_002,
            atimensec: 123,
            mtimensec: 456,
            ctimensec: 789,
            mode: 0o644,
            unused4: 0,
            uid: 1000,
            gid: 1000,
            unused5: 0,
        };
        assert_eq!(FuseSetattrIn::from_bytes(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn setattr_in_field_offsets_match_spec() {
        // Use distinct sentinel values per field group.
        let h = FuseSetattrIn {
            valid: 0x0101_0101,                // [0..4]
            padding: 0x0202_0202,              // [4..8]
            fh: 0x0303_0303_0303_0303,         // [8..16]
            size: 0x0404_0404_0404_0404,       // [16..24]
            lock_owner: 0x0505_0505_0505_0505, // [24..32]
            atime: 0x0606_0606_0606_0606,      // [32..40]
            mtime: 0x0707_0707_0707_0707,      // [40..48]
            ctime: 0x0808_0808_0808_0808,      // [48..56]
            atimensec: 0x0909_0909,            // [56..60]
            mtimensec: 0x0A0A_0A0A,            // [60..64]
            ctimensec: 0x0B0B_0B0B,            // [64..68]
            mode: 0x0C0C_0C0C,                 // [68..72]
            unused4: 0x0D0D_0D0D,              // [72..76]
            uid: 0x0E0E_0E0E,                  // [76..80]
            gid: 0x0F0F_0F0F,                  // [80..84]
            unused5: 0x1010_1010,              // [84..88]
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..4], &[0x01; 4]);
        assert_eq!(&b[4..8], &[0x02; 4]);
        assert_eq!(&b[8..16], &[0x03; 8]);
        assert_eq!(&b[16..24], &[0x04; 8]);
        assert_eq!(&b[24..32], &[0x05; 8]);
        assert_eq!(&b[32..40], &[0x06; 8]);
        assert_eq!(&b[40..48], &[0x07; 8]);
        assert_eq!(&b[48..56], &[0x08; 8]);
        assert_eq!(&b[56..60], &[0x09; 4]);
        assert_eq!(&b[60..64], &[0x0A; 4]);
        assert_eq!(&b[64..68], &[0x0B; 4]);
        assert_eq!(&b[68..72], &[0x0C; 4]);
        assert_eq!(&b[72..76], &[0x0D; 4]);
        assert_eq!(&b[76..80], &[0x0E; 4]);
        assert_eq!(&b[80..84], &[0x0F; 4]);
        assert_eq!(&b[84..88], &[0x10; 4]);
    }

    #[test]
    fn setattr_in_rejects_short_input() {
        let short = [0u8; 87];
        assert!(matches!(
            FuseSetattrIn::from_bytes(&short),
            Err(FuseError::ShortHeader { have: 87, need: 88 })
        ));
    }

    #[test]
    fn fattr_constants_are_distinct_bits() {
        let all = [
            fattr::MODE,
            fattr::UID,
            fattr::GID,
            fattr::SIZE,
            fattr::ATIME,
            fattr::MTIME,
            fattr::FH,
            fattr::ATIME_NOW,
            fattr::MTIME_NOW,
            fattr::LOCKOWNER,
            fattr::CTIME,
            fattr::KILL_SUIDGID,
        ];
        // Each value is a unique power-of-two.
        for (i, a) in all.iter().enumerate() {
            assert!(
                a.is_power_of_two(),
                "fattr constant at index {i} is not a power of two"
            );
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "fattr constants at indices {i} and {j} collide");
                }
            }
        }
    }

    // ---- FuseStatfsOut -----------------------------------------------

    #[test]
    fn statfs_out_length_is_80() {
        assert_eq!(FUSE_STATFS_OUT_LEN, 80);
    }

    #[test]
    fn statfs_out_roundtrips_all_fields() {
        let h = FuseStatfsOut {
            blocks: 1_000_000,
            bfree: 500_000,
            bavail: 499_000,
            files: 100_000,
            ffree: 99_000,
            bsize: 4096,
            namelen: 255,
            frsize: 4096,
            padding: 0,
            spare: [1, 2, 3, 4, 5, 6],
        };
        assert_eq!(FuseStatfsOut::from_bytes(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn statfs_out_field_offsets_match_spec() {
        let h = FuseStatfsOut {
            blocks: 0x0101_0101_0101_0101, // [0..8]
            bfree: 0x0202_0202_0202_0202,  // [8..16]
            bavail: 0x0303_0303_0303_0303, // [16..24]
            files: 0x0404_0404_0404_0404,  // [24..32]
            ffree: 0x0505_0505_0505_0505,  // [32..40]
            bsize: 0x0606_0606,            // [40..44]
            namelen: 0x0707_0707,          // [44..48]
            frsize: 0x0808_0808,           // [48..52]
            padding: 0x0909_0909,          // [52..56]
            spare: [0x0A0A_0A0A; 6],       // [56..80]
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..8], &[0x01; 8]);
        assert_eq!(&b[8..16], &[0x02; 8]);
        assert_eq!(&b[16..24], &[0x03; 8]);
        assert_eq!(&b[24..32], &[0x04; 8]);
        assert_eq!(&b[32..40], &[0x05; 8]);
        assert_eq!(&b[40..44], &[0x06; 4]);
        assert_eq!(&b[44..48], &[0x07; 4]);
        assert_eq!(&b[48..52], &[0x08; 4]);
        assert_eq!(&b[52..56], &[0x09; 4]);
        assert_eq!(&b[56..80], &[0x0A; 24]);
    }

    #[test]
    fn statfs_out_rejects_short_input() {
        let short = [0u8; 79];
        assert!(matches!(
            FuseStatfsOut::from_bytes(&short),
            Err(FuseError::ShortHeader { have: 79, need: 80 })
        ));
    }

    // ---- FuseMknodIn / FuseMkdirIn -----------------------------------

    #[test]
    fn mknod_in_length_is_16() {
        assert_eq!(FUSE_MKNOD_IN_LEN, 16);
    }

    #[test]
    fn mknod_in_roundtrips() {
        let h = FuseMknodIn {
            mode: 0o060644, // block device (S_IFBLK) + rw-r--r--
            rdev: (8 << 8), // major 8 (sda), minor 0
            umask: 0o022,
            padding: 0,
        };
        assert_eq!(FuseMknodIn::from_bytes(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn mknod_in_field_offsets_match_spec() {
        let h = FuseMknodIn {
            mode: 0x0101_0101,
            rdev: 0x0202_0202,
            umask: 0x0303_0303,
            padding: 0x0404_0404,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..4], &[0x01; 4]);
        assert_eq!(&b[4..8], &[0x02; 4]);
        assert_eq!(&b[8..12], &[0x03; 4]);
        assert_eq!(&b[12..16], &[0x04; 4]);
    }

    #[test]
    fn mknod_in_rejects_short_input() {
        let short = [0u8; 15];
        assert!(matches!(
            FuseMknodIn::from_bytes(&short),
            Err(FuseError::ShortHeader { have: 15, need: 16 })
        ));
    }

    #[test]
    fn mkdir_in_length_is_8() {
        assert_eq!(FUSE_MKDIR_IN_LEN, 8);
    }

    #[test]
    fn mkdir_in_roundtrips() {
        let h = FuseMkdirIn {
            mode: 0o755,
            umask: 0o022,
        };
        assert_eq!(FuseMkdirIn::from_bytes(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn mkdir_in_field_offsets_match_spec() {
        let h = FuseMkdirIn {
            mode: 0x0101_0101,
            umask: 0x0202_0202,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..4], &[0x01; 4]);
        assert_eq!(&b[4..8], &[0x02; 4]);
    }

    #[test]
    fn mkdir_in_rejects_short_input() {
        let short = [0u8; 7];
        assert!(matches!(
            FuseMkdirIn::from_bytes(&short),
            Err(FuseError::ShortHeader { have: 7, need: 8 })
        ));
    }

    // ---- FuseFsyncIn -------------------------------------------------

    #[test]
    fn fsync_in_length_is_16() {
        assert_eq!(FUSE_FSYNC_IN_LEN, 16);
    }

    #[test]
    fn fsync_in_roundtrips() {
        let h = FuseFsyncIn {
            fh: 0xBEEF_1234,
            fsync_flags: FUSE_FSYNC_FDATASYNC,
            padding: 0,
        };
        assert_eq!(FuseFsyncIn::from_bytes(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn fsync_in_field_offsets_match_spec() {
        let h = FuseFsyncIn {
            fh: 0x0101_0101_0101_0101,
            fsync_flags: 0x0202_0202,
            padding: 0x0303_0303,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..8], &[0x01; 8]);
        assert_eq!(&b[8..12], &[0x02; 4]);
        assert_eq!(&b[12..16], &[0x03; 4]);
    }

    #[test]
    fn fsync_fdatasync_flag_is_bit_zero() {
        assert_eq!(FUSE_FSYNC_FDATASYNC, 1);
    }

    #[test]
    fn fsync_in_rejects_short_input() {
        let short = [0u8; 15];
        assert!(matches!(
            FuseFsyncIn::from_bytes(&short),
            Err(FuseError::ShortHeader { have: 15, need: 16 })
        ));
    }

    // ---- FuseRenameIn ------------------------------------------------

    #[test]
    fn rename_in_length_is_8() {
        assert_eq!(FUSE_RENAME_IN_LEN, 8);
    }

    #[test]
    fn rename_in_roundtrips() {
        let h = FuseRenameIn {
            newdir: 0xDEAD_CAFE_0000_0001,
        };
        assert_eq!(FuseRenameIn::from_bytes(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn rename_in_field_offset_matches_spec() {
        let h = FuseRenameIn {
            newdir: 0x0101_0101_0101_0101,
        };
        assert_eq!(&h.to_bytes()[0..8], &[0x01u8; 8]);
    }

    #[test]
    fn rename_in_rejects_short_input() {
        let short = [0u8; 7];
        assert!(matches!(
            FuseRenameIn::from_bytes(&short),
            Err(FuseError::ShortHeader { have: 7, need: 8 })
        ));
    }

    // ---- FuseLinkIn --------------------------------------------------

    #[test]
    fn link_in_length_is_8() {
        assert_eq!(FUSE_LINK_IN_LEN, 8);
    }

    #[test]
    fn link_in_roundtrips() {
        let h = FuseLinkIn {
            oldnodeid: 0xBEEF_1234_ABCD_0001,
        };
        assert_eq!(FuseLinkIn::from_bytes(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn link_in_field_offset_matches_spec() {
        let h = FuseLinkIn {
            oldnodeid: 0x0202_0202_0202_0202,
        };
        assert_eq!(&h.to_bytes()[0..8], &[0x02u8; 8]);
    }

    #[test]
    fn link_in_rejects_short_input() {
        let short = [0u8; 7];
        assert!(matches!(
            FuseLinkIn::from_bytes(&short),
            Err(FuseError::ShortHeader { have: 7, need: 8 })
        ));
    }

    // ---- fopen constants ---------------------------------------------

    #[test]
    fn fopen_constants_are_distinct_bits() {
        let all = [
            fopen::DIRECT_IO,
            fopen::KEEP_CACHE,
            fopen::NONSEEKABLE,
            fopen::CACHE_DIR,
            fopen::STREAM,
            fopen::NOFLUSH,
        ];
        for (i, a) in all.iter().enumerate() {
            assert!(
                a.is_power_of_two(),
                "fopen constant at index {i} is not a power of two"
            );
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "fopen constants at indices {i} and {j} collide");
                }
            }
        }
        // Pin the values against the kernel's fuse.h.
        assert_eq!(fopen::DIRECT_IO, 1 << 0);
        assert_eq!(fopen::KEEP_CACHE, 1 << 1);
        assert_eq!(fopen::NONSEEKABLE, 1 << 2);
        assert_eq!(fopen::CACHE_DIR, 1 << 3);
        assert_eq!(fopen::STREAM, 1 << 4);
        assert_eq!(fopen::NOFLUSH, 1 << 5);
    }
}
