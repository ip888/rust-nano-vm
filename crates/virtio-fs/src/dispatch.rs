//! FUSE request dispatch scaffolding.
//!
//! This module bridges the wire format (byte slices from virtqueue descriptor
//! chains) and host-side filesystem logic:
//!
//! 1. **Parse** — [`parse_request`] reads a raw FUSE packet into a
//!    [`FuseRequest`] enum that carries already-decoded headers and body
//!    structs (and zero-copy byte-slice references for variable-length
//!    payloads such as file names and write data).
//!
//! 2. **Handle** — callers implement the [`FuseHandler`] trait. Every method
//!    has a default implementation that returns [`ENOSYS`], so an
//!    incomplete handler compiles immediately and the guest sees a clean
//!    "not implemented" error rather than a hang or panic.
//!
//! 3. **Dispatch** — [`dispatch`] calls the right [`FuseHandler`] method and
//!    serialises the response (or returns `None` for no-reply ops such as
//!    `Forget`).
//!
//! # Wire layout
//!
//! Every inbound FUSE packet is:
//!
//! ```text
//! [FuseInHeader (40 bytes)] [body struct (0..N bytes)] [payload (0..M bytes)]
//! ```
//!
//! The body struct size is opcode-specific and fixed; the trailing payload
//! is variable (file name / write data / ...).  [`parse_request`] slices the
//! buffer accordingly and stores payload as a `&[u8]` lifetime-tied to the
//! original buffer so no copies are needed at the parse step.
//!
//! # No-KVM guarantee
//!
//! Everything in this module is pure byte-slice manipulation — no kernel,
//! no KVM, no unsafe code. All types and functions are unit-testable with
//! synthetic buffers on any machine.

use crate::{
    FuseError, FuseInHeader, FuseOpcode, FuseOutHeader, FUSE_IN_HDR_LEN, FUSE_OUT_HDR_LEN,
};
use crate::{
    FuseAttrOut, FuseEntryOut, FuseFlushIn, FuseForgetIn, FuseFsyncIn, FuseGetattrIn, FuseInitIn,
    FuseInitOut, FuseLinkIn, FuseMkdirIn, FuseMknodIn, FuseOpenIn, FuseOpenOut, FuseReadIn,
    FuseReleaseIn, FuseRenameIn, FuseSetattrIn, FuseStatfsOut, FuseWriteIn, FuseWriteOut,
};
use crate::{
    FUSE_LINK_IN_LEN, FUSE_MKDIR_IN_LEN, FUSE_MKNOD_IN_LEN, FUSE_RENAME_IN_LEN, FUSE_WRITE_IN_LEN,
};

// ---------------------------------------------------------------------------
// errno constants (we avoid a libc dependency in this crate)
// ---------------------------------------------------------------------------

/// POSIX `ENOSYS` — function not implemented. Returned by all default
/// [`FuseHandler`] methods so callers that only partially implement the
/// trait still compile and the guest sees a clean error.
pub const ENOSYS: i32 = 38;

/// POSIX `EINVAL` — invalid argument.
pub const EINVAL: i32 = 22;

// ---------------------------------------------------------------------------
// FuseRequest<'a>
// ---------------------------------------------------------------------------

/// A fully-parsed inbound FUSE request, ready for dispatch.
///
/// Lifetime `'a` is tied to the input byte slice passed to
/// [`parse_request`]. Variants that carry variable-length payloads (file
/// names, write data) hold zero-copy `&'a [u8]` references into that
/// slice.
///
/// # Note on NUL terminators
///
/// FUSE name payloads are NUL-terminated C strings. The slices here
/// include the trailing NUL so the handler can verify it is present if
/// needed, but handlers typically call
/// [`split_nul`] to separate the meaningful bytes from the terminator.
#[derive(Debug)]
#[non_exhaustive]
pub enum FuseRequest<'a> {
    /// `FUSE_INIT` — protocol handshake, first packet after mount.
    Init(FuseInitIn),

    /// `FUSE_FORGET` — kernel releases `nlookup` references to `nodeid`.
    /// **No reply must be sent to the guest.**
    Forget(FuseForgetIn),

    /// `FUSE_GETATTR` — read inode attributes. Response: [`FuseAttrOut`].
    Getattr(FuseGetattrIn),

    /// `FUSE_SETATTR` — modify inode attributes. Response: [`FuseAttrOut`].
    Setattr(FuseSetattrIn),

    /// `FUSE_LOOKUP` — resolve `name` within `nodeid`. Response: [`FuseEntryOut`].
    Lookup {
        /// NUL-terminated name component to look up.
        name: &'a [u8],
    },

    /// `FUSE_READLINK` — read symlink target. Response: raw target bytes
    /// (no NUL terminator on the wire).
    Readlink,

    /// `FUSE_MKNOD` — create special file. Response: [`FuseEntryOut`].
    Mknod {
        /// Fixed-size mknod parameters.
        body: FuseMknodIn,
        /// NUL-terminated file name.
        name: &'a [u8],
    },

    /// `FUSE_MKDIR` — create directory. Response: [`FuseEntryOut`].
    Mkdir {
        /// Fixed-size mkdir parameters.
        body: FuseMkdirIn,
        /// NUL-terminated directory name.
        name: &'a [u8],
    },

    /// `FUSE_UNLINK` — remove file. Empty response.
    Unlink {
        /// NUL-terminated name to unlink.
        name: &'a [u8],
    },

    /// `FUSE_RMDIR` — remove empty directory. Empty response.
    Rmdir {
        /// NUL-terminated directory name.
        name: &'a [u8],
    },

    /// `FUSE_SYMLINK` — create symlink. Response: [`FuseEntryOut`].
    Symlink {
        /// NUL-terminated new symlink name.
        name: &'a [u8],
        /// NUL-terminated symlink target path.
        target: &'a [u8],
    },

    /// `FUSE_RENAME` — rename / move. Empty response.
    Rename {
        /// Fixed-size rename parameters (contains the target directory nodeid).
        body: FuseRenameIn,
        /// NUL-terminated source name.
        old_name: &'a [u8],
        /// NUL-terminated destination name.
        new_name: &'a [u8],
    },

    /// `FUSE_LINK` — create hard link. Response: [`FuseEntryOut`].
    Link {
        /// Fixed-size link parameters (contains the source inode).
        body: FuseLinkIn,
        /// NUL-terminated name for the new link.
        name: &'a [u8],
    },

    /// `FUSE_OPEN` — open regular file. Response: [`FuseOpenOut`].
    Open(FuseOpenIn),

    /// `FUSE_READ` — read from open file handle. Response: raw data bytes.
    Read(FuseReadIn),

    /// `FUSE_WRITE` — write to open file handle. Response: [`FuseWriteOut`].
    Write {
        /// Fixed-size write parameters.
        body: FuseWriteIn,
        /// Data bytes to write (`body.size` bytes).
        data: &'a [u8],
    },

    /// `FUSE_STATFS` — query filesystem statistics. Response: [`FuseStatfsOut`].
    Statfs,

    /// `FUSE_RELEASE` — close regular-file handle. Empty response.
    Release(FuseReleaseIn),

    /// `FUSE_FSYNC` — sync file handle to backing store. Empty response.
    Fsync(FuseFsyncIn),

    /// `FUSE_FLUSH` — flush write buffer before close. Empty response.
    Flush(FuseFlushIn),

    /// `FUSE_OPENDIR` — open directory handle. Response: [`FuseOpenOut`].
    Opendir(FuseOpenIn),

    /// `FUSE_READDIR` — list directory entries. Response: raw dirent bytes.
    Readdir(FuseReadIn),

    /// `FUSE_RELEASEDIR` — close directory handle. Empty response.
    Releasedir(FuseReleaseIn),

    /// `FUSE_DESTROY` — orderly session shutdown.
    /// **No reply must be sent to the guest.**
    Destroy,
}

// ---------------------------------------------------------------------------
// parse_request
// ---------------------------------------------------------------------------

/// Parse a complete raw FUSE request packet from `buf`.
///
/// `buf` must contain at least [`FUSE_IN_HDR_LEN`] bytes; the body and
/// optional payload follow immediately.
///
/// Returns `(header, request)` on success.
///
/// # Errors
///
/// - [`FuseError::ShortHeader`] — `buf` is shorter than 40 bytes.
/// - [`FuseError::UnknownOpcode`] — opcode not recognised.
/// - [`FuseError::ShortHeader`] — body is shorter than the fixed body struct
///   for this opcode (reuses the same variant; `need` gives the minimum
///   total packet length expected).
pub fn parse_request(buf: &[u8]) -> Result<(FuseInHeader, FuseRequest<'_>), FuseError> {
    let hdr = FuseInHeader::from_bytes(buf)?;
    let body = &buf[FUSE_IN_HDR_LEN..];
    let req = match hdr.opcode {
        FuseOpcode::Init => FuseRequest::Init(FuseInitIn::from_bytes(body)?),
        FuseOpcode::Forget => FuseRequest::Forget(FuseForgetIn::from_bytes(body)?),
        FuseOpcode::Getattr => FuseRequest::Getattr(FuseGetattrIn::from_bytes(body)?),
        FuseOpcode::Setattr => FuseRequest::Setattr(FuseSetattrIn::from_bytes(body)?),
        FuseOpcode::Lookup => FuseRequest::Lookup { name: body },
        FuseOpcode::Readlink => FuseRequest::Readlink,
        FuseOpcode::Mknod => {
            let b = FuseMknodIn::from_bytes(body)?;
            FuseRequest::Mknod {
                body: b,
                name: &body[FUSE_MKNOD_IN_LEN..],
            }
        }
        FuseOpcode::Mkdir => {
            let b = FuseMkdirIn::from_bytes(body)?;
            FuseRequest::Mkdir {
                body: b,
                name: &body[FUSE_MKDIR_IN_LEN..],
            }
        }
        FuseOpcode::Unlink => FuseRequest::Unlink { name: body },
        FuseOpcode::Rmdir => FuseRequest::Rmdir { name: body },
        FuseOpcode::Symlink => {
            // Two NUL-terminated strings: name \0 target \0
            let (name, target) = split_two_nul_strings(body)?;
            FuseRequest::Symlink { name, target }
        }
        FuseOpcode::Rename => {
            let b = FuseRenameIn::from_bytes(body)?;
            let rest = &body[FUSE_RENAME_IN_LEN..];
            let (old_name, new_name) = split_two_nul_strings(rest)?;
            FuseRequest::Rename {
                body: b,
                old_name,
                new_name,
            }
        }
        FuseOpcode::Link => {
            let b = FuseLinkIn::from_bytes(body)?;
            FuseRequest::Link {
                body: b,
                name: &body[FUSE_LINK_IN_LEN..],
            }
        }
        FuseOpcode::Open => FuseRequest::Open(FuseOpenIn::from_bytes(body)?),
        FuseOpcode::Read => FuseRequest::Read(FuseReadIn::from_bytes(body)?),
        FuseOpcode::Write => {
            let b = FuseWriteIn::from_bytes(body)?;
            let data = &body[FUSE_WRITE_IN_LEN..];
            FuseRequest::Write { body: b, data }
        }
        FuseOpcode::Statfs => FuseRequest::Statfs,
        FuseOpcode::Release => FuseRequest::Release(FuseReleaseIn::from_bytes(body)?),
        FuseOpcode::Fsync => FuseRequest::Fsync(FuseFsyncIn::from_bytes(body)?),
        FuseOpcode::Flush => FuseRequest::Flush(FuseFlushIn::from_bytes(body)?),
        FuseOpcode::Opendir => FuseRequest::Opendir(FuseOpenIn::from_bytes(body)?),
        FuseOpcode::Readdir => FuseRequest::Readdir(FuseReadIn::from_bytes(body)?),
        FuseOpcode::Releasedir => FuseRequest::Releasedir(FuseReleaseIn::from_bytes(body)?),
        FuseOpcode::Destroy => FuseRequest::Destroy,
    };
    Ok((hdr, req))
}

/// Split a byte slice at the first NUL, returning `(before_nul, after_nul)`.
///
/// Returns `Err(FuseError::ShortHeader { have: buf.len(), need: 1 })` if
/// there is no NUL in `buf`.  The NUL itself is not included in either
/// slice.
pub fn split_nul(buf: &[u8]) -> Result<(&[u8], &[u8]), FuseError> {
    buf.iter()
        .position(|&b| b == 0)
        .map(|pos| (&buf[..pos], &buf[pos + 1..]))
        .ok_or(FuseError::ShortHeader {
            have: buf.len(),
            need: buf.len() + 1,
        })
}

/// Split a byte slice containing two consecutive NUL-terminated strings,
/// returning `(first_string, second_string)` (each excluding their NUL).
///
/// Returns an error if either NUL terminator is absent.
fn split_two_nul_strings(buf: &[u8]) -> Result<(&[u8], &[u8]), FuseError> {
    let (first, rest) = split_nul(buf)?;
    let (second, _) = split_nul(rest)?;
    Ok((first, second))
}

// ---------------------------------------------------------------------------
// FuseHandler trait
// ---------------------------------------------------------------------------

/// Host-side filesystem handler. Implement this trait to service FUSE
/// requests parsed by [`parse_request`].
///
/// Every method has a default implementation returning `Err(ENOSYS)` (or a
/// no-op for no-reply ops), so a partial implementation compiles immediately
/// and the guest sees a clean "function not implemented" error for anything
/// not yet wired up.
///
/// The `hdr: &FuseInHeader` argument is passed to every method so
/// implementations can access caller identity (`uid`, `gid`, `pid`) and the
/// `unique` id without each method signature duplicating those fields.
#[allow(unused_variables)]
pub trait FuseHandler {
    /// `FUSE_INIT` — protocol handshake. Return the negotiated
    /// [`FuseInitOut`] or an errno.
    fn init(&mut self, hdr: &FuseInHeader, req: &FuseInitIn) -> Result<FuseInitOut, i32> {
        Err(ENOSYS)
    }

    /// `FUSE_FORGET` — kernel releases references to `nodeid`. No reply
    /// is ever sent; the default implementation is a no-op.
    fn forget(&mut self, hdr: &FuseInHeader, req: &FuseForgetIn) {}

    /// `FUSE_GETATTR` — return inode attributes.
    fn getattr(&mut self, hdr: &FuseInHeader, req: &FuseGetattrIn) -> Result<FuseAttrOut, i32> {
        Err(ENOSYS)
    }

    /// `FUSE_SETATTR` — modify inode attributes, return updated attributes.
    fn setattr(&mut self, hdr: &FuseInHeader, req: &FuseSetattrIn) -> Result<FuseAttrOut, i32> {
        Err(ENOSYS)
    }

    /// `FUSE_LOOKUP` — resolve `name` within `hdr.nodeid`. `name` is the
    /// raw NUL-terminated bytes; use [`split_nul`] to strip the terminator.
    fn lookup(&mut self, hdr: &FuseInHeader, name: &[u8]) -> Result<FuseEntryOut, i32> {
        Err(ENOSYS)
    }

    /// `FUSE_READLINK` — return the target of the symlink at `hdr.nodeid`.
    /// The response is the raw target bytes (no NUL).
    fn readlink(&mut self, hdr: &FuseInHeader) -> Result<Vec<u8>, i32> {
        Err(ENOSYS)
    }

    /// `FUSE_MKNOD` — create special file `name` in `hdr.nodeid`.
    fn mknod(
        &mut self,
        hdr: &FuseInHeader,
        req: &FuseMknodIn,
        name: &[u8],
    ) -> Result<FuseEntryOut, i32> {
        Err(ENOSYS)
    }

    /// `FUSE_MKDIR` — create directory `name` in `hdr.nodeid`.
    fn mkdir(
        &mut self,
        hdr: &FuseInHeader,
        req: &FuseMkdirIn,
        name: &[u8],
    ) -> Result<FuseEntryOut, i32> {
        Err(ENOSYS)
    }

    /// `FUSE_UNLINK` — remove file `name` from `hdr.nodeid`.
    fn unlink(&mut self, hdr: &FuseInHeader, name: &[u8]) -> Result<(), i32> {
        Err(ENOSYS)
    }

    /// `FUSE_RMDIR` — remove empty directory `name` from `hdr.nodeid`.
    fn rmdir(&mut self, hdr: &FuseInHeader, name: &[u8]) -> Result<(), i32> {
        Err(ENOSYS)
    }

    /// `FUSE_SYMLINK` — create symlink `name` → `target` in `hdr.nodeid`.
    fn symlink(
        &mut self,
        hdr: &FuseInHeader,
        name: &[u8],
        target: &[u8],
    ) -> Result<FuseEntryOut, i32> {
        Err(ENOSYS)
    }

    /// `FUSE_RENAME` — rename `old_name` to `new_name`, moving into the
    /// directory at `req.newdir` if different from `hdr.nodeid`.
    fn rename(
        &mut self,
        hdr: &FuseInHeader,
        req: &FuseRenameIn,
        old_name: &[u8],
        new_name: &[u8],
    ) -> Result<(), i32> {
        Err(ENOSYS)
    }

    /// `FUSE_LINK` — create a hard link to `req.oldnodeid` named `name` in
    /// `hdr.nodeid`.
    fn link(
        &mut self,
        hdr: &FuseInHeader,
        req: &FuseLinkIn,
        name: &[u8],
    ) -> Result<FuseEntryOut, i32> {
        Err(ENOSYS)
    }

    /// `FUSE_OPEN` — open regular file at `hdr.nodeid`.
    fn open(&mut self, hdr: &FuseInHeader, req: &FuseOpenIn) -> Result<FuseOpenOut, i32> {
        Err(ENOSYS)
    }

    /// `FUSE_READ` — read from `req.fh`. Return the data bytes (up to
    /// `req.size` bytes; an empty `Ok(vec![])` is a valid EOF response).
    fn read(&mut self, hdr: &FuseInHeader, req: &FuseReadIn) -> Result<Vec<u8>, i32> {
        Err(ENOSYS)
    }

    /// `FUSE_WRITE` — write `data` to `req.fh` at `req.offset`.
    fn write(
        &mut self,
        hdr: &FuseInHeader,
        req: &FuseWriteIn,
        data: &[u8],
    ) -> Result<FuseWriteOut, i32> {
        Err(ENOSYS)
    }

    /// `FUSE_STATFS` — return filesystem statistics.
    fn statfs(&mut self, hdr: &FuseInHeader) -> Result<FuseStatfsOut, i32> {
        Err(ENOSYS)
    }

    /// `FUSE_RELEASE` — close regular-file handle `req.fh`.
    fn release(&mut self, hdr: &FuseInHeader, req: &FuseReleaseIn) -> Result<(), i32> {
        Err(ENOSYS)
    }

    /// `FUSE_FSYNC` — flush data (and optionally metadata) for `req.fh` to
    /// the backing store.
    fn fsync(&mut self, hdr: &FuseInHeader, req: &FuseFsyncIn) -> Result<(), i32> {
        Err(ENOSYS)
    }

    /// `FUSE_FLUSH` — flush cached writes before the file descriptor is
    /// closed on the guest side.
    fn flush(&mut self, hdr: &FuseInHeader, req: &FuseFlushIn) -> Result<(), i32> {
        Err(ENOSYS)
    }

    /// `FUSE_OPENDIR` — open directory at `hdr.nodeid`.
    fn opendir(&mut self, hdr: &FuseInHeader, req: &FuseOpenIn) -> Result<FuseOpenOut, i32> {
        Err(ENOSYS)
    }

    /// `FUSE_READDIR` — list directory entries starting at `req.offset`.
    /// Return raw `fuse_dirent` bytes (use [`FuseDirentWriter`] to build
    /// them); an empty `Ok(vec![])` signals end-of-directory.
    fn readdir(&mut self, hdr: &FuseInHeader, req: &FuseReadIn) -> Result<Vec<u8>, i32> {
        Err(ENOSYS)
    }

    /// `FUSE_RELEASEDIR` — close directory handle `req.fh`.
    fn releasedir(&mut self, hdr: &FuseInHeader, req: &FuseReleaseIn) -> Result<(), i32> {
        Err(ENOSYS)
    }

    /// `FUSE_DESTROY` — orderly session shutdown. No reply is sent; the
    /// default implementation is a no-op.
    fn destroy(&mut self, hdr: &FuseInHeader) {}
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

/// Dispatch a parsed request to `handler`, serialise the response, and
/// return it.
///
/// Returns `None` for operations that never send a reply to the guest
/// (`Forget`, `Destroy`). Returns `Some(response_bytes)` for all other
/// operations; on handler error `response_bytes` is a
/// `FuseOutHeader::err(...)` with no body.
///
/// The returned `Vec<u8>` is ready to write directly into the device-
/// writable descriptor chain.
pub fn dispatch<H: FuseHandler>(
    hdr: &FuseInHeader,
    req: FuseRequest<'_>,
    handler: &mut H,
) -> Option<Vec<u8>> {
    match req {
        // ---- no-reply ops ------------------------------------------------
        FuseRequest::Forget(ref body) => {
            handler.forget(hdr, body);
            None
        }
        FuseRequest::Destroy => {
            handler.destroy(hdr);
            None
        }

        // ---- ops with fixed-size response bodies -------------------------
        FuseRequest::Init(ref body) => {
            encode_result(hdr.unique, handler.init(hdr, body).map(|r| r.to_bytes().to_vec()))
        }
        FuseRequest::Getattr(ref body) => encode_result(
            hdr.unique,
            handler.getattr(hdr, body).map(|r| r.to_bytes().to_vec()),
        ),
        FuseRequest::Setattr(ref body) => encode_result(
            hdr.unique,
            handler.setattr(hdr, body).map(|r| r.to_bytes().to_vec()),
        ),
        FuseRequest::Lookup { name } => {
            encode_result(hdr.unique, handler.lookup(hdr, name).map(|r| r.to_bytes().to_vec()))
        }
        FuseRequest::Readlink => {
            encode_result(hdr.unique, handler.readlink(hdr))
        }
        FuseRequest::Mknod { ref body, name } => encode_result(
            hdr.unique,
            handler.mknod(hdr, body, name).map(|r| r.to_bytes().to_vec()),
        ),
        FuseRequest::Mkdir { ref body, name } => encode_result(
            hdr.unique,
            handler.mkdir(hdr, body, name).map(|r| r.to_bytes().to_vec()),
        ),
        FuseRequest::Unlink { name } => {
            encode_result(hdr.unique, handler.unlink(hdr, name).map(|()| vec![]))
        }
        FuseRequest::Rmdir { name } => {
            encode_result(hdr.unique, handler.rmdir(hdr, name).map(|()| vec![]))
        }
        FuseRequest::Symlink { name, target } => encode_result(
            hdr.unique,
            handler.symlink(hdr, name, target).map(|r| r.to_bytes().to_vec()),
        ),
        FuseRequest::Rename {
            ref body,
            old_name,
            new_name,
        } => encode_result(
            hdr.unique,
            handler.rename(hdr, body, old_name, new_name).map(|()| vec![]),
        ),
        FuseRequest::Link { ref body, name } => encode_result(
            hdr.unique,
            handler.link(hdr, body, name).map(|r| r.to_bytes().to_vec()),
        ),
        FuseRequest::Open(ref body) => {
            encode_result(hdr.unique, handler.open(hdr, body).map(|r| r.to_bytes().to_vec()))
        }
        FuseRequest::Read(ref body) => {
            encode_result(hdr.unique, handler.read(hdr, body))
        }
        FuseRequest::Write { ref body, data } => encode_result(
            hdr.unique,
            handler.write(hdr, body, data).map(|r| r.to_bytes().to_vec()),
        ),
        FuseRequest::Statfs => {
            encode_result(hdr.unique, handler.statfs(hdr).map(|r| r.to_bytes().to_vec()))
        }
        FuseRequest::Release(ref body) => {
            encode_result(hdr.unique, handler.release(hdr, body).map(|()| vec![]))
        }
        FuseRequest::Fsync(ref body) => {
            encode_result(hdr.unique, handler.fsync(hdr, body).map(|()| vec![]))
        }
        FuseRequest::Flush(ref body) => {
            encode_result(hdr.unique, handler.flush(hdr, body).map(|()| vec![]))
        }
        FuseRequest::Opendir(ref body) => {
            encode_result(hdr.unique, handler.opendir(hdr, body).map(|r| r.to_bytes().to_vec()))
        }
        FuseRequest::Readdir(ref body) => {
            encode_result(hdr.unique, handler.readdir(hdr, body))
        }
        FuseRequest::Releasedir(ref body) => {
            encode_result(hdr.unique, handler.releasedir(hdr, body).map(|()| vec![]))
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Encode a handler result into a complete FUSE response packet.
///
/// On `Ok(body_bytes)` → [`FuseOutHeader::ok`] + body bytes.
/// On `Err(errno)` → [`FuseOutHeader::err`] (no body).
///
/// `errno` should be a **positive** POSIX error number (e.g. `ENOSYS = 38`).
/// [`FuseOutHeader::err`] stores the negated value in the wire field so that
/// the guest sees a negative errno, matching the FUSE wire convention.
/// [`i32::unsigned_abs`] is used defensively so an accidentally-negated errno
/// still encodes correctly.
fn encode_result(unique: u64, result: Result<Vec<u8>, i32>) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(FUSE_OUT_HDR_LEN + 128);
    match result {
        Ok(body) => {
            let hdr = FuseOutHeader::ok(unique, body.len() as u32);
            out.extend_from_slice(&hdr.to_bytes());
            out.extend_from_slice(&body);
        }
        Err(errno) => {
            let hdr = FuseOutHeader::err(unique, errno.unsigned_abs());
            out.extend_from_slice(&hdr.to_bytes());
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FuseInitIn, FuseInitOut, FuseOpcode, FUSE_INIT_IN_LEN, FUSE_INIT_OUT_LEN,
        FUSE_IN_HDR_LEN, FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION, FUSE_OUT_HDR_LEN,
    };

    // ---- helpers ---------------------------------------------------------

    /// Build a minimal [`FuseInHeader`] with the given opcode, `unique=1`,
    /// all other fields zeroed.
    fn make_hdr(opcode: FuseOpcode, body_len: u32) -> [u8; FUSE_IN_HDR_LEN] {
        let mut buf = [0u8; FUSE_IN_HDR_LEN];
        let total = FUSE_IN_HDR_LEN as u32 + body_len;
        buf[0..4].copy_from_slice(&total.to_le_bytes());
        buf[4..8].copy_from_slice(&opcode.as_raw().to_le_bytes());
        buf[8..16].copy_from_slice(&1u64.to_le_bytes()); // unique = 1
        buf
    }

    /// Concatenate a header slice and a body slice into one `Vec<u8>`.
    fn packet(hdr: &[u8], body: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(hdr.len() + body.len());
        v.extend_from_slice(hdr);
        v.extend_from_slice(body);
        v
    }

    // ---- parse_request ---------------------------------------------------

    #[test]
    fn parse_init_roundtrips_body() {
        let body_in = FuseInitIn {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 65536,
            flags: 0,
        };
        let pkt = packet(
            &make_hdr(FuseOpcode::Init, FUSE_INIT_IN_LEN as u32),
            &body_in.to_bytes(),
        );
        let (_hdr, req) = parse_request(&pkt).unwrap();
        match req {
            FuseRequest::Init(parsed) => assert_eq!(parsed, body_in),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_forget_roundtrips_body() {
        use crate::{FuseForgetIn, FUSE_FORGET_IN_LEN};
        let body_in = FuseForgetIn { nlookup: 42 };
        let pkt = packet(
            &make_hdr(FuseOpcode::Forget, FUSE_FORGET_IN_LEN as u32),
            &body_in.to_bytes(),
        );
        let (_hdr, req) = parse_request(&pkt).unwrap();
        match req {
            FuseRequest::Forget(parsed) => assert_eq!(parsed, body_in),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_lookup_carries_name_bytes() {
        let name = b"hello\0";
        let pkt = packet(
            &make_hdr(FuseOpcode::Lookup, name.len() as u32),
            name,
        );
        let (_hdr, req) = parse_request(&pkt).unwrap();
        match req {
            FuseRequest::Lookup { name: parsed_name } => assert_eq!(parsed_name, name),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_readlink_has_no_body() {
        let pkt = make_hdr(FuseOpcode::Readlink, 0);
        let (_hdr, req) = parse_request(&pkt).unwrap();
        assert!(matches!(req, FuseRequest::Readlink));
    }

    #[test]
    fn parse_statfs_has_no_body() {
        let pkt = make_hdr(FuseOpcode::Statfs, 0);
        let (_hdr, req) = parse_request(&pkt).unwrap();
        assert!(matches!(req, FuseRequest::Statfs));
    }

    #[test]
    fn parse_destroy_has_no_body() {
        let pkt = make_hdr(FuseOpcode::Destroy, 0);
        let (_hdr, req) = parse_request(&pkt).unwrap();
        assert!(matches!(req, FuseRequest::Destroy));
    }

    #[test]
    fn parse_symlink_splits_two_nul_strings() {
        let payload = b"mylink\0/usr/lib/libc.so\0";
        let pkt = packet(
            &make_hdr(FuseOpcode::Symlink, payload.len() as u32),
            payload,
        );
        let (_hdr, req) = parse_request(&pkt).unwrap();
        match req {
            FuseRequest::Symlink { name, target } => {
                assert_eq!(name, b"mylink");
                assert_eq!(target, b"/usr/lib/libc.so");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_write_attaches_data_slice() {
        use crate::{FuseWriteIn, FUSE_WRITE_IN_LEN};
        let write_body = FuseWriteIn {
            fh: 7,
            offset: 0,
            size: 5,
            ..Default::default()
        };
        let data = b"hello";
        let mut body_bytes = vec![0u8; FUSE_WRITE_IN_LEN];
        write_body.write_to(&mut body_bytes).unwrap();
        body_bytes.extend_from_slice(data);
        let pkt = packet(
            &make_hdr(FuseOpcode::Write, body_bytes.len() as u32),
            &body_bytes,
        );
        let (_hdr, req) = parse_request(&pkt).unwrap();
        match req {
            FuseRequest::Write { body, data: parsed_data } => {
                assert_eq!(body.fh, 7);
                assert_eq!(parsed_data, data.as_slice());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_request_rejects_short_buffer() {
        let short = [0u8; FUSE_IN_HDR_LEN - 1];
        let err = parse_request(&short).unwrap_err();
        assert!(matches!(err, FuseError::ShortHeader { .. }));
    }

    #[test]
    fn parse_request_rejects_unknown_opcode() {
        let mut buf = [0u8; FUSE_IN_HDR_LEN];
        buf[4..8].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        let err = parse_request(&buf).unwrap_err();
        assert!(matches!(err, FuseError::UnknownOpcode(0xDEAD_BEEF)));
    }

    // ---- split_nul -------------------------------------------------------

    #[test]
    fn split_nul_finds_first_terminator() {
        let (before, after) = split_nul(b"foo\0bar").unwrap();
        assert_eq!(before, b"foo");
        assert_eq!(after, b"bar");
    }

    #[test]
    fn split_nul_on_empty_name_before_nul() {
        let (before, after) = split_nul(b"\0rest").unwrap();
        assert_eq!(before, b"");
        assert_eq!(after, b"rest");
    }

    #[test]
    fn split_nul_errors_when_no_nul_present() {
        let err = split_nul(b"no-nul-here").unwrap_err();
        assert!(matches!(err, FuseError::ShortHeader { .. }));
    }

    // ---- dispatch --------------------------------------------------------

    /// A test handler that answers Init with a known InitOut and records
    /// Forget calls.
    struct TestHandler {
        forget_count: u64,
    }

    impl FuseHandler for TestHandler {
        fn init(&mut self, _hdr: &FuseInHeader, _req: &FuseInitIn) -> Result<FuseInitOut, i32> {
            Ok(FuseInitOut {
                major: FUSE_KERNEL_VERSION,
                minor: FUSE_KERNEL_MINOR_VERSION,
                max_readahead: 65536,
                flags: 0,
                max_background: 0,
                congestion_threshold: 0,
                max_write: 4096,
                time_gran: 1,
                max_pages: 1,
                map_alignment: 0,
                flags2: 0,
                unused: [0u32; 7],
            })
        }

        fn forget(&mut self, _hdr: &FuseInHeader, req: &FuseForgetIn) {
            self.forget_count += req.nlookup;
        }
    }

    #[test]
    fn dispatch_init_returns_encoded_response() {
        let body_in = FuseInitIn {
            major: 7,
            minor: 33,
            max_readahead: 65536,
            flags: 0,
        };
        let pkt = packet(
            &make_hdr(FuseOpcode::Init, FUSE_INIT_IN_LEN as u32),
            &body_in.to_bytes(),
        );
        let (hdr, req) = parse_request(&pkt).unwrap();
        let mut handler = TestHandler { forget_count: 0 };
        let resp = dispatch(&hdr, req, &mut handler).expect("Init should produce a reply");

        // Parse the response header.
        let out_hdr = FuseOutHeader::from_bytes(&resp).unwrap();
        assert_eq!(out_hdr.error, 0, "success response");
        assert_eq!(out_hdr.unique, 1, "echoes request unique");
        assert_eq!(
            out_hdr.len as usize,
            FUSE_OUT_HDR_LEN + FUSE_INIT_OUT_LEN,
            "total len includes body"
        );
    }

    #[test]
    fn dispatch_forget_returns_none() {
        use crate::{FuseForgetIn, FUSE_FORGET_IN_LEN};
        let body = FuseForgetIn { nlookup: 3 };
        let pkt = packet(
            &make_hdr(FuseOpcode::Forget, FUSE_FORGET_IN_LEN as u32),
            &body.to_bytes(),
        );
        let (hdr, req) = parse_request(&pkt).unwrap();
        let mut handler = TestHandler { forget_count: 0 };
        let resp = dispatch(&hdr, req, &mut handler);
        assert!(resp.is_none(), "Forget must not produce a reply");
        assert_eq!(handler.forget_count, 3);
    }

    #[test]
    fn dispatch_destroy_returns_none() {
        let pkt = make_hdr(FuseOpcode::Destroy, 0);
        let (hdr, req) = parse_request(&pkt).unwrap();
        let mut handler = TestHandler { forget_count: 0 };
        let resp = dispatch(&hdr, req, &mut handler);
        assert!(resp.is_none(), "Destroy must not produce a reply");
    }

    #[test]
    fn dispatch_unimplemented_handler_returns_enosys() {
        // Default handler → ENOSYS for Getattr.
        use crate::{FuseGetattrIn, FUSE_GETATTR_IN_LEN};

        struct NullHandler;
        impl FuseHandler for NullHandler {}

        let body = FuseGetattrIn::default();
        let mut body_bytes = [0u8; FUSE_GETATTR_IN_LEN];
        body.write_to(&mut body_bytes).unwrap();
        let pkt = packet(
            &make_hdr(FuseOpcode::Getattr, FUSE_GETATTR_IN_LEN as u32),
            &body_bytes,
        );
        let (hdr, req) = parse_request(&pkt).unwrap();
        let resp = dispatch(&hdr, req, &mut NullHandler).unwrap();
        let out_hdr = FuseOutHeader::from_bytes(&resp).unwrap();
        assert_eq!(out_hdr.error, -ENOSYS, "should be -ENOSYS");
        // Error response must contain only the 16-byte header, no body.
        assert_eq!(resp.len(), FUSE_OUT_HDR_LEN);
    }

    #[test]
    fn encode_result_ok_prepends_header() {
        let body = vec![1u8, 2, 3, 4];
        let raw = encode_result(42, Ok(body.clone())).unwrap();
        let hdr = FuseOutHeader::from_bytes(&raw).unwrap();
        assert_eq!(hdr.unique, 42);
        assert_eq!(hdr.error, 0);
        assert_eq!(hdr.len as usize, FUSE_OUT_HDR_LEN + 4);
        assert_eq!(&raw[FUSE_OUT_HDR_LEN..], body.as_slice());
    }

    #[test]
    fn encode_result_err_emits_header_only() {
        let raw = encode_result(7, Err(EINVAL)).unwrap();
        assert_eq!(raw.len(), FUSE_OUT_HDR_LEN);
        let hdr = FuseOutHeader::from_bytes(&raw).unwrap();
        assert_eq!(hdr.unique, 7);
        assert_eq!(hdr.error, -EINVAL);
    }
}
