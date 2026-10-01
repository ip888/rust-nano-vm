//! Integration tests that exercise the real Linux TAP path:
//! open an interface via `/dev/net/tun` + `TUNSETIFF`, verify the
//! readiness fd and the non-blocking read behaviour. The write side
//! requires an attached bridge (the kernel otherwise reports EIO on
//! a down interface) and lands with sub-PR #D's host networking
//! plumbing — this file does not cover frame transfer.
//!
//! Skipped (returns 0 passed, 0 failed) on hosts without
//! `CAP_NET_ADMIN` or without the TUN module. Follows the same
//! skip-when-fixtures-missing pattern the `crates/vm-kvm` real-KVM
//! integration tests use.

#![cfg(target_os = "linux")]

use std::path::Path;

use virtio_net::{NetworkBackend, TapDevice};

/// Capability + device probe. Returns `true` when the test should
/// skip, `false` when the environment has what we need.
///
/// We can't just check for `/dev/net/tun` existence — the character
/// device is readable to any user, but the TUNSETIFF ioctl inside
/// `TapDevice::open` requires `CAP_NET_ADMIN`. GitHub Actions
/// runners are the classic case: TUN module loaded, device present,
/// but no capability. Probe by actually opening a short-lived TAP
/// and detecting only the error kinds that mean "environment can't
/// do this": `PermissionDenied` (EPERM / EACCES from the ioctl).
///
/// Any other error — bad ioctl request number, invalid flags,
/// resource exhaustion, ENODEV — bubbles up as a real test failure
/// so a regression in `TapDevice::open` can't masquerade as a skip.
/// `EBUSY` from the exclusive-creation flag is the one exception:
/// seeing EBUSY proves the ioctl succeeded past the capability
/// check, so we have the capability and should run the test — the
/// specific probe name was just already taken by a parallel test
/// run (cargo runs integration tests in parallel threads).
fn skip_if_no_tun() -> bool {
    if !Path::new("/dev/net/tun").exists() {
        eprintln!("skip: /dev/net/tun not present — kernel without TUN");
        return true;
    }
    // Unique probe name so parallel tests don't race on the same
    // interface. Thread id + PID gives per-run uniqueness.
    let probe_name = format!("np-{}", thread_tag());
    match TapDevice::open(&probe_name) {
        Ok(_) => false,
        Err(virtio_net::VirtioNetError::Io(e))
            if e.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            eprintln!("skip: TAP probe EPERM — missing CAP_NET_ADMIN");
            true
        }
        // EBUSY means the ioctl passed the capability check and
        // failed on IFF_TUN_EXCL finding an existing interface
        // with the same name. That proves we have the capability.
        Err(virtio_net::VirtioNetError::Io(e)) if e.raw_os_error() == Some(libc::EBUSY) => false,
        Err(e) => panic!("TAP probe failed in an unexpected way: {e:?}"),
    }
}

/// Short, unique-per-thread tag for interface names so a cargo-test
/// parallel run doesn't race on IFF_TUN_EXCL. 11 bytes max because
/// Linux IFNAMSIZ caps names at 15 bytes and the test prefix uses
/// up some of that.
fn thread_tag() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    std::thread::current().id().hash(&mut hasher);
    format!("{:x}", hasher.finish() & 0xffff)
}

#[test]
fn open_close_tap_smoke() {
    if skip_if_no_tun() {
        return;
    }
    let name = format!("nvm-t0-{}", thread_tag());
    let tap = TapDevice::open(&name).expect("open TAP");
    assert_eq!(tap.name(), &name);
    let fd = tap.readiness_fd().expect("Some(fd)");
    assert!(fd >= 0, "readiness_fd must be a non-negative descriptor");
}

#[test]
fn tap_read_returns_zero_when_no_traffic() {
    if skip_if_no_tun() {
        return;
    }
    let name = format!("nvm-t1-{}", thread_tag());
    let tap = TapDevice::open(&name).expect("open TAP");
    let mut buf = [0u8; 2048];
    let n = tap.read_frame(&mut buf).expect("read");
    assert_eq!(n, 0);
}

// Write-through-a-detached-TAP is deliberately NOT tested here.
// The interface starts administratively DOWN; writing to a down
// TAP returns EIO on some kernels and Ok(silently dropped) on
// others. Bringing the interface up requires SIOCSIFFLAGS which
// sub-PR #D adds along with the bridge/NAT plumbing. Once that
// lands, a proper end-to-end write test lives in the vm-kvm
// integration suite where we can attach both ends.

#[test]
fn readiness_fd_is_a_valid_pollable_fd() {
    if skip_if_no_tun() {
        return;
    }
    let name = format!("nvm-t3-{}", thread_tag());
    let tap = TapDevice::open(&name).expect("open TAP");
    let rfd = tap.readiness_fd().expect("Some(fd)");
    let mut pfd = libc::pollfd {
        fd: rfd,
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
    assert!(
        rc >= 0,
        "poll rc={rc} errno={}",
        std::io::Error::last_os_error()
    );
    assert_eq!(
        pfd.revents & libc::POLLNVAL,
        0,
        "poll reported POLLNVAL — the readiness_fd is not a valid descriptor"
    );
    drop(tap);
}
