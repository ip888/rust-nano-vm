//! # `SmoltcpBackend` — host-side userspace TCP/IP stack
//!
//! > **Terminology:** every term here (TCP/IP stack, Ethernet frame,
//! > IP, TCP, UDP, MAC, …) is defined once in the terminology table
//! > at the top of `lib.rs`.
//!
//! Where [`crate::TapDevice`] hands frames to the Linux kernel's
//! network stack (bridge → iptables NAT → wire), **`SmoltcpBackend`
//! runs the TCP/IP stack in our own process** using the
//! [`smoltcp`](https://docs.rs/smoltcp) crate. The practical
//! consequences:
//!
//! - **No `CAP_NET_ADMIN` needed.** No `TUNSETIFF`, no `ip link`,
//!   no `iptables` — everything happens in userspace. Works in a
//!   rootless container, in a non-privileged Fly.io machine, in a
//!   standard EKS pod.
//! - **Snapshot-friendly.** All per-guest network state (TCP sockets,
//!   their buffers, ARP cache, timers) lives in our Rust-level
//!   structs. `bincode::serialize(&backend)` captures it; restore
//!   just replays. The Linux kernel's netns + bridge port + iptables
//!   chain, which the TAP backend relies on, is **not** safely
//!   serializable.
//!
//! ## Architecture at a glance
//!
//! ```text
//!      HOST SIDE                                 GUEST SIDE
//!   ───────────────                             ─────────────
//!    Browser → host socket :443                 Spring Boot :8080
//!         │                                          │
//!         ▼                                          │
//!    (TCP byte proxy)                                │
//!         │                                          │
//!         ▼                                          │
//!    ┌─────────────────────┐                         │
//!    │ smoltcp::Interface  │                         │
//!    │   ├ IP: host-side   │                         │
//!    │   ├ ARP cache       │  Ethernet frames        │
//!    │   ├ TCP sockets     │  through virtio-net     │
//!    │   └ RX/TX queues    │  ring (sub-PR #B.2)     │
//!    └─────────────────────┘                         │
//!         ▲        │                                 │
//!         │        │                                 │
//!         │        ▼                                 │
//!    (write_frame) (read_frame)   ◄── NetworkBackend ──►
//! ```
//!
//! Guest kernel's `virtio_net` driver does **not** know we're
//! userspace — it still speaks the virtio-net protocol, same wire
//! bytes as if we were a kernel TAP backend. The whole difference
//! lives on the host side.
//!
//! ## This commit (sub-PR #D commit 1/6)
//!
//! Lands just the skeleton — the dependency, the module registration,
//! and a `SmoltcpBackend` struct that implements [`NetworkBackend`]
//! as a frame-queue pass-through (same shape as [`crate::MockBackend`]
//! but with the intent of being upgraded in-place). The real smoltcp
//! `Interface` + socket set land in commit 2.
//!
//! Gated behind the `smoltcp-backend` crate feature so users on the
//! kernel-TAP path aren't forced to depend on smoltcp. Enabled via
//! `virtio-net = { version = "...", features = ["smoltcp-backend"] }`
//! in a downstream crate's Cargo.toml.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::{NetworkBackend, Result};

/// Default IPv4 address advertised on the host side of the
/// `SmoltcpBackend`. Picked from the IANA link-local range
/// (169.254.0.0/16) which is guaranteed never to collide with
/// real-world public IPs the guest might legitimately reach.
///
/// Guests get `169.254.0.2`; host side (us) gets `169.254.0.1`.
/// This range is unroutable through real NICs — if a guest somehow
/// bypasses virtio-net and reaches a host NIC with these addresses,
/// the Linux kernel drops the frame, so no leakage.
pub const DEFAULT_HOST_IPV4: [u8; 4] = [169, 254, 0, 1];
/// Guest-side IPv4 the `SmoltcpBackend` expects the guest to use.
/// Must match the IP the guest's networking is configured with (via
/// DHCP in later commits, or hardcoded in cmdline for v1 demo).
pub const DEFAULT_GUEST_IPV4: [u8; 4] = [169, 254, 0, 2];

/// Userspace TCP/IP backend for virtio-net.
///
/// Commit 1/6 stub: this struct holds two frame queues and routes
/// bytes between them with no actual TCP/IP processing. Commits
/// 2-4 progressively bolt on a `smoltcp::iface::Interface`, socket
/// bindings, and host-side TCP proxy threads so that bytes written
/// to the guest's virtio-net ring come back out of a host `:443`
/// socket as part of a TCP connection.
///
/// # Rust concept: `Mutex<T>` for frame-queue synchronisation
///
/// Both `read_frame(&self, ...)` and `write_frame(&self, ...)` on
/// the [`NetworkBackend`] trait take a shared `&self` receiver so
/// the virtio device can hand the same `Arc<dyn NetworkBackend>` to
/// its TX drain path and its RX fill path. To mutate the internal
/// queues through a shared reference we need **interior mutability**
/// — standard idiom: wrap the mutable state in a `Mutex<T>`.
///
/// A `RwLock` would let read-heavy loads scale better, but our
/// access pattern alternates reads and writes 1-for-1 (every
/// `write_frame` is almost immediately followed by a `read_frame`
/// as smoltcp generates the TCP ACK), so a plain `Mutex` is simpler
/// and just as fast in practice.
#[derive(Debug)]
pub struct SmoltcpBackend {
    inner: Mutex<State>,
}

/// All mutable state the backend holds, kept behind one `Mutex` to
/// avoid lock-ordering concerns between queue mutation and (later)
/// smoltcp `Interface` polling.
#[derive(Debug, Default)]
struct State {
    /// Frames queued for delivery to the guest. The virtio queue
    /// consumer's `process_rx` pops from here on every RX kick.
    /// Produced by host-side activity in commits 2-4; in this
    /// commit it stays empty.
    rx: VecDeque<Vec<u8>>,
    /// Frames the guest's `virtio_net_xmit` has produced. The
    /// virtio queue consumer's `process_tx` pushes here. Commit 2
    /// will feed these into the `smoltcp::Interface` for parsing.
    tx: VecDeque<Vec<u8>>,
}

impl SmoltcpBackend {
    /// Build a new backend. In commit 1 this is a zero-config
    /// constructor; commits 2+ grow a `SmoltcpConfig` struct with
    /// IP / port / MAC overrides so callers can swap for test or
    /// production profiles.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(State::default()),
        }
    }

    /// Push a frame into the RX queue (as if it came from the
    /// outside world). Used by host-side threads in later commits
    /// when a host-socket read produced bytes smoltcp wants to
    /// send to the guest. Also useful in unit tests.
    pub fn inject_rx(&self, frame: Vec<u8>) {
        self.inner
            .lock()
            .expect("smoltcp backend state mutex poisoned")
            .rx
            .push_back(frame);
    }

    /// Pop the oldest guest-produced frame from the TX queue, if
    /// any. The real integration with smoltcp happens in commit 2;
    /// this helper is useful for the commit 1 unit tests that
    /// prove the queue-pass-through contract matches `MockBackend`.
    pub fn pop_tx(&self) -> Option<Vec<u8>> {
        self.inner
            .lock()
            .expect("smoltcp backend state mutex poisoned")
            .tx
            .pop_front()
    }

    /// Depths of the two internal queues `(rx, tx)`. Test helper.
    pub fn queue_depths(&self) -> (usize, usize) {
        let s = self
            .inner
            .lock()
            .expect("smoltcp backend state mutex poisoned");
        (s.rx.len(), s.tx.len())
    }
}

impl Default for SmoltcpBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkBackend for SmoltcpBackend {
    /// Read one Ethernet frame from the backend into `buf`.
    ///
    /// In commit 1 this just pops from the internal RX queue; the
    /// semantics mirror `MockBackend::read_frame` exactly, including
    /// the Linux-TAP truncate-and-consume behaviour (if `buf` is
    /// smaller than the next frame, we copy as much as fits and
    /// drop the rest). Commits 2+ will poll the `smoltcp::Interface`
    /// here first so that TCP responses generated by host-side
    /// activity make it to the guest.
    fn read_frame(&self, buf: &mut [u8]) -> Result<usize> {
        let mut state = self
            .inner
            .lock()
            .expect("smoltcp backend state mutex poisoned");
        match state.rx.pop_front() {
            None => Ok(0),
            Some(frame) => {
                let n = frame.len().min(buf.len());
                buf[..n].copy_from_slice(&frame[..n]);
                Ok(n)
            }
        }
    }

    /// Write one Ethernet frame to the backend.
    ///
    /// In commit 1 this just pushes onto the internal TX queue so
    /// unit tests can observe what the guest produced. Commit 2
    /// adds a `smoltcp::Interface::device_mut().receive()` call so
    /// the frame gets parsed and routed to the correct host-side
    /// TCP socket.
    fn write_frame(&self, frame: &[u8]) -> Result<()> {
        self.inner
            .lock()
            .expect("smoltcp backend state mutex poisoned")
            .tx
            .push_back(frame.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_backend_has_empty_queues() {
        let bk = SmoltcpBackend::new();
        assert_eq!(bk.queue_depths(), (0, 0));
    }

    #[test]
    fn write_frame_lands_on_tx_queue() {
        let bk = SmoltcpBackend::new();
        let frame = b"hello ethernet".to_vec();
        bk.write_frame(&frame).unwrap();
        assert_eq!(bk.queue_depths(), (0, 1));
        assert_eq!(bk.pop_tx().as_deref(), Some(&frame[..]));
        assert_eq!(bk.pop_tx(), None);
    }

    #[test]
    fn inject_rx_produces_a_read() {
        let bk = SmoltcpBackend::new();
        let injected = (0..64u8).collect::<Vec<_>>();
        bk.inject_rx(injected.clone());

        let mut buf = [0u8; 2048];
        let n = bk.read_frame(&mut buf).unwrap();
        assert_eq!(n, 64);
        assert_eq!(&buf[..n], &injected[..]);
    }

    #[test]
    fn read_frame_returns_zero_when_empty() {
        let bk = SmoltcpBackend::new();
        let mut buf = [0u8; 64];
        assert_eq!(bk.read_frame(&mut buf).unwrap(), 0);
    }

    #[test]
    fn oversized_frame_is_truncated_like_tap_and_mock() {
        // Must match the contract MockBackend asserts in its own
        // test suite — a short buffer truncates and consumes.
        let bk = SmoltcpBackend::new();
        bk.inject_rx(vec![0xAB; 1500]);

        let mut small = [0u8; 64];
        let n = bk.read_frame(&mut small).unwrap();
        assert_eq!(n, 64);
        assert_eq!(small, [0xAB; 64]);

        // Frame consumed — a bigger buffer sees empty.
        let mut big = [0u8; 2048];
        assert_eq!(bk.read_frame(&mut big).unwrap(), 0);
    }

    #[test]
    fn default_ipv4_constants_are_in_link_local_range() {
        // Link-local range per RFC 3927 is 169.254.0.0/16. If a
        // future commit tweaks these constants, this test flags
        // any value that falls outside the range (which would
        // risk collision with a real-world routable IP the guest
        // might legitimately reach).
        assert_eq!(DEFAULT_HOST_IPV4[0..2], [169, 254]);
        assert_eq!(DEFAULT_GUEST_IPV4[0..2], [169, 254]);
        assert_ne!(DEFAULT_HOST_IPV4, DEFAULT_GUEST_IPV4);
    }
}
