//! Linux TAP-backed [`NetworkBackend`].
//!
//! [`TapDevice`] opens `/dev/net/tun`, issues `TUNSETIFF` to bind
//! it to a named interface, and provides read/write of raw
//! Ethernet frames through that fd. This is the entire "host side
//! transport" for the virtio-net device — the device from sub-PR #B
//! holds a `Box<dyn NetworkBackend>` which in production is a
//! `TapDevice`.
//!
//! # What TAP is (again, quickly)
//!
//! `/dev/net/tun` is a character device that lets userspace act as
//! a virtual network card. After you open it and call
//! `TUNSETIFF` with a name, Linux creates an interface (e.g.
//! `nanovm-tap-42`) visible via `ip link`. Read the fd → get the
//! next Ethernet frame that the kernel routed to that interface.
//! Write to the fd → the kernel treats the bytes as an Ethernet
//! frame arriving from "the wire".
//!
//! The interface starts down; upstack code (sub-PR #D) will bring
//! it up, assign an IP, add it to a bridge.

// `unsafe` is required here for two reasons:
//   1. `libc::ioctl` is a variadic C function — no way to type it
//      safely in Rust.
//   2. `TUNSETIFF` takes a pointer to an `ifreq` struct we fill
//      out; we hand libc a raw `*mut ifreq` that outlives the call.
//
// Both are limited to `open_tap`. Everything else operates on a
// fully-owned `RawFd` via `read(2)` / `write(2)` which have safe
// wrappers in `std::io`.
#![allow(unsafe_code)]

use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::Mutex;

use crate::{NetworkBackend, Result, VirtioNetError};

// Linux caps interface names at IFNAMSIZ = 16 (includes trailing
// NUL). See `include/uapi/linux/if.h`. Deriving this from
// `libc::IFNAMSIZ` would be cleaner but `libc` doesn't expose it
// as a `const` — it's a `usize` on Linux but the API is version-
// dependent. Hard-coding 16 has been stable since Linux 1.0.
const IFNAMSIZ: usize = 16;

// TUNSETIFF ioctl request. Value from `include/uapi/linux/if_tun.h`.
// `_IOW('T', 202, int)` = 0x400454ca. We could derive this via
// `nix::ioc::iow!` but pinning the constant keeps `nix` out of the
// dependency tree.
const TUNSETIFF: libc::c_ulong = 0x400454ca;

// Flags for the `ifr_flags` field of `ifreq` when passed to
// TUNSETIFF. `IFF_TAP` — L2 device (Ethernet frames). `IFF_NO_PI` —
// don't prepend a 4-byte "packet info" header; we want raw frames.
// `IFF_TUN_EXCL` — require creation of a brand-new interface; refuse
// to attach to an existing persistent TAP that happens to have the
// requested name. Without this flag Linux may silently bind our fd
// to an old interface and the caller never knows.
const IFF_TAP: libc::c_short = 0x0002;
const IFF_NO_PI: libc::c_short = 0x1000;
const IFF_TUN_EXCL: libc::c_short = 0x8000u16 as libc::c_short;

// Struct layout matches the kernel's `struct ifreq` used with
// `TUNSETIFF`. The full C definition is a union of many variants —
// we only care about `ifr_name` + `ifr_flags`, so we pack a matching
// prefix. `repr(C)` guarantees the struct layout matches the C ABI
// (no field reordering, natural padding, C-compatible enum sizes).
#[repr(C)]
struct Ifreq {
    ifr_name: [libc::c_char; IFNAMSIZ],
    ifr_flags: libc::c_short,
    // Pad to `sizeof(struct ifreq)` = 40 on x86-64 Linux so the
    // kernel doesn't read uninitialised memory past the union.
    _pad: [u8; 22],
}

/// Linux TAP-backed [`NetworkBackend`].
///
/// # Rust concept: RAII resource ownership
///
/// The `Drop` trait implementation on [`TapDevice`] guarantees that
/// closing the fd happens *exactly once* — when the value goes out
/// of scope, even on panic. C code has to remember to `close(fd)`
/// on every error path; forgetting is how file-descriptor leaks
/// happen. Rust's borrow checker + `Drop` makes forgetting a
/// compile-time or runtime-obvious bug rather than a slow leak.
///
/// We keep the fd inside a `File` (not a bare `RawFd`) so `Drop`
/// on `File` closes it for us — one less place to write manual
/// `libc::close`.
#[derive(Debug)]
pub struct TapDevice {
    // `File` inside a `Mutex` because reads and writes must be
    // exclusive on the fd — concurrent `read()`s on the same TUN
    // fd read the same frame in Linux (each frame is consumed
    // atomically, so two threads racing would each get a valid
    // frame but both would advance the internal state; ordering
    // becomes surprising). The Mutex serialises access; the virtio
    // device runs TX and RX from separate threads and this makes
    // them safe.
    file: Mutex<File>,

    // Retained for logging and later teardown. `ip link del`
    // deletes the interface; `Drop` on the fd removes the kernel-
    // side reference, and the interface disappears automatically
    // (persist flag not set), but keeping the name is handy for
    // debug prints.
    name: String,
}

impl TapDevice {
    /// Open a new TAP interface with the given name.
    ///
    /// Requires `CAP_NET_ADMIN` on the calling process (root, or
    /// explicit capability). Returns the wrapper on success, or a
    /// [`VirtioNetError::Io`] on any syscall failure.
    ///
    /// # Errors
    ///
    /// - Name too long (> 15 bytes) or contains an interior NUL.
    /// - `/dev/net/tun` doesn't exist (kernel not built with TUN,
    ///   or `modprobe tun` not run).
    /// - Missing `CAP_NET_ADMIN` — kernel returns `EPERM`.
    /// - Interface with the same name already exists — `EBUSY`.
    pub fn open(name: &str) -> Result<Self> {
        if name.is_empty() || name.len() >= IFNAMSIZ {
            return Err(VirtioNetError::InvalidIfName {
                name: name.to_string(),
                reason: "must be 1..15 bytes",
            });
        }
        if name.as_bytes().contains(&0) {
            return Err(VirtioNetError::InvalidIfName {
                name: name.to_string(),
                reason: "contains interior NUL",
            });
        }

        // `CString::new` copies + appends NUL; enforces no interior
        // NUL (already checked above but belt-and-braces).
        let cname = CString::new(name).map_err(|_| VirtioNetError::InvalidIfName {
            name: name.to_string(),
            reason: "contains interior NUL",
        })?;

        // Zero-init the whole request struct so no uninitialised
        // bytes leak into the kernel's read of the ifreq union.
        // `mem::zeroed()` is unsafe because it produces a value of
        // any type by writing 0s; for our `#[repr(C)]` struct with
        // all-integer fields this is a safe zero-value.
        let mut ifr: Ifreq = unsafe { mem::zeroed() };
        ifr.ifr_flags = IFF_TAP | IFF_NO_PI | IFF_TUN_EXCL;
        for (i, &b) in cname.as_bytes().iter().enumerate() {
            ifr.ifr_name[i] = b as libc::c_char;
        }

        // Open the character device with O_RDWR (we need both) and
        // O_NONBLOCK so `read()` returns EAGAIN rather than blocking
        // — the virtio device wants to drive this with `epoll`, not
        // block a thread on it.
        let path = CString::new("/dev/net/tun").expect("no NUL");
        let flags = libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC;
        // SAFETY: `path` is a valid C-string pointer that outlives
        // the syscall (until the end of this expression). `flags` is
        // a POSIX flag word. `open` returns -1 on failure and sets
        // errno; we surface that via `io::Error::last_os_error`.
        let fd = unsafe { libc::open(path.as_ptr(), flags) };
        if fd < 0 {
            return Err(VirtioNetError::Io(std::io::Error::last_os_error()));
        }
        // Wrap the fd in a `File` immediately so any early return
        // below closes it via Drop rather than leaking.
        // SAFETY: `fd` is a fresh, owned fd from `open`; we haven't
        // handed it to anyone else and take ownership here.
        let file = unsafe { File::from_raw_fd(fd) };

        // Issue TUNSETIFF. The kernel reads `ifr_name` and `ifr_flags`,
        // creates the interface, and returns 0 on success.
        // SAFETY: `ioctl` is variadic FFI; the third argument's type
        // depends on the request. `TUNSETIFF` expects `*mut ifreq`.
        // Our `Ifreq` matches the kernel's layout for the fields it
        // reads; the padding covers the tail of the union.
        let rc = unsafe { libc::ioctl(file.as_raw_fd(), TUNSETIFF, &mut ifr as *mut _) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            // `file` drops here, closing the fd.
            return Err(VirtioNetError::Io(err));
        }

        Ok(Self {
            file: Mutex::new(file),
            name: name.to_string(),
        })
    }

    /// The interface name Linux created (matches the argument to
    /// [`Self::open`]). Handy for logs and for `ip link set …`
    /// commands the host-side setup runs later.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Consume the wrapper and return the raw fd. Used only by
    /// tests that need to hand the fd to another syscall
    /// (e.g. `getsockname`); production code sticks to the safe
    /// trait methods.
    ///
    /// After this call, `Drop` no longer runs on the file — the
    /// caller is responsible for closing.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn into_raw_fd(self) -> std::os::fd::RawFd {
        use std::os::fd::IntoRawFd;
        // `into_inner()` unwraps the Mutex once — sound because we
        // consume `self`, so no other holder exists.
        self.file.into_inner().unwrap().into_raw_fd()
    }
}

impl NetworkBackend for TapDevice {
    fn read_frame(&self, buf: &mut [u8]) -> Result<usize> {
        let mut file = self.file.lock().unwrap();
        // `read` returns 0 for EOF on a regular file; for a TUN fd,
        // EOF doesn't happen. `EAGAIN` from a non-blocking fd shows
        // up as `io::ErrorKind::WouldBlock` — treat as "no frame".
        match file.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(VirtioNetError::Io(e)),
        }
    }

    fn write_frame(&self, frame: &[u8]) -> Result<()> {
        let mut file = self.file.lock().unwrap();
        // `write_all` retries partial writes; TUN accepts frames
        // atomically (all or nothing), so a partial return would
        // mean a short frame, which is a kernel bug. Still, using
        // `write_all` is one less footgun.
        file.write_all(frame).map_err(VirtioNetError::Io)
    }

    fn readiness_fd(&self) -> Option<i32> {
        // Return the raw fd. Caller must not close it — they'll
        // just poll() / epoll() on it for readability. Passing the
        // fd by number rather than by owned `RawFd` type is
        // intentional (see trait doc).
        Some(self.file.lock().unwrap().as_raw_fd())
    }
}

#[cfg(test)]
mod tests {
    // Real TAP tests require CAP_NET_ADMIN. Under `cargo test` they
    // land in an integration test at `tests/tap_integration.rs`
    // which is skipped when caps are missing. This inline module
    // covers only the pure-Rust logic (name validation, error
    // shape) that doesn't touch the kernel.
    use super::*;

    #[test]
    fn empty_name_rejected() {
        let err = TapDevice::open("").unwrap_err();
        match err {
            VirtioNetError::InvalidIfName { reason, .. } => {
                assert!(reason.contains("1..15"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn oversize_name_rejected() {
        let too_long = "a".repeat(IFNAMSIZ); // exactly 16 → rejected
        let err = TapDevice::open(&too_long).unwrap_err();
        match err {
            VirtioNetError::InvalidIfName { reason, .. } => {
                assert!(reason.contains("1..15"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn interior_nul_name_rejected() {
        let err = TapDevice::open("bad\0name").unwrap_err();
        match err {
            VirtioNetError::InvalidIfName { reason, .. } => {
                assert!(reason.contains("NUL"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
