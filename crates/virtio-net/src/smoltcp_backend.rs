//! # `SmoltcpBackend` — host-side userspace TCP/IP stack
//!
//! > **Terminology:** every term here (TCP/IP stack, Ethernet frame,
//! > IP, TCP, UDP, MAC, ARP, …) is defined once in the terminology
//! > table at the top of `lib.rs`.
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
//!   structs. The Linux kernel's netns + bridge port + iptables
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
//!    (TCP byte proxy — commit #3-4)                  │
//!         │                                          │
//!         ▼                                          │
//!    ┌─────────────────────┐                         │
//!    │ smoltcp::Interface  │                         │
//!    │   ├ IP: 169.254.0.1 │                         │
//!    │   ├ ARP cache       │  Ethernet frames        │
//!    │   ├ TCP sockets     │  through virtio-net     │
//!    │   │  (commit #3+)   │  ring (sub-PR #B.2)     │
//!    │   └ RX/TX queues    │                         │
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
//! ## This commit (sub-PR #D commit 2/6)
//!
//! Lands the real `smoltcp::phy::Device` + `smoltcp::iface::Interface`:
//!
//! - A `SmoltDevice` struct implementing `smoltcp::phy::Device`,
//!   backed by two in-memory `VecDeque<Vec<u8>>` frame queues.
//! - An `Interface` built on top with our host-side IPv4 address
//!   and a default route for the guest-side IP.
//! - `NetworkBackend::write_frame` now feeds the inbound queue and
//!   polls the interface, so smoltcp can parse the frame and
//!   generate replies (ARP response, ICMP echo-reply, …).
//! - `NetworkBackend::read_frame` polls the interface and pops any
//!   outbound frames the stack produced.
//!
//! After this commit a guest that sends `arp who-has 169.254.0.1`
//! gets back our host MAC, and `ping 169.254.0.1` from inside the
//! guest round-trips. TCP sockets land in commit #3; the host-side
//! `:443` byte proxy lands in commit #4.
//!
//! Gated behind the `smoltcp-backend` crate feature so users on the
//! kernel-TAP path aren't forced to depend on smoltcp.

use std::collections::VecDeque;
use std::sync::Mutex;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address};

use crate::{NetworkBackend, Result};

/// Default IPv4 address advertised on the host side of the
/// `SmoltcpBackend`. Picked from the IANA link-local range
/// (169.254.0.0/16) which is guaranteed never to collide with
/// real-world public IPs the guest might legitimately reach.
pub const DEFAULT_HOST_IPV4: [u8; 4] = [169, 254, 0, 1];

/// Guest-side IPv4 the `SmoltcpBackend` expects the guest to use.
/// Must match the IP the guest's networking is configured with
/// (via DHCP in later commits, or hardcoded in cmdline for v1 demo).
pub const DEFAULT_GUEST_IPV4: [u8; 4] = [169, 254, 0, 2];

/// Host-side MAC address advertised by the smoltcp interface.
/// Locally-administered OUI (`52:54:00:...`) is the QEMU convention
/// — same prefix the virtio-net device uses for guest MACs. Last
/// three bytes distinct from the guest's MAC so ARP works
/// unambiguously.
pub const DEFAULT_HOST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0xff, 0xff, 0x01];

/// IPv4 prefix length (/16 for link-local 169.254.0.0/16).
const HOST_IPV4_PREFIX_LEN: u8 = 16;

/// Max Ethernet frame bytes the device accepts/produces (same as
/// [`crate::MAX_FRAME_LEN`] — 1518 with 802.1Q VLAN overhead).
const DEVICE_MTU: usize = 1518;

/// Userspace TCP/IP backend for virtio-net.
///
/// # Rust concept: `Mutex<T>` for interior mutability
///
/// Both `read_frame(&self, ...)` and `write_frame(&self, ...)` on
/// the [`NetworkBackend`] trait take a shared `&self` receiver so
/// the virtio device can hand the same `Arc<dyn NetworkBackend>` to
/// its TX drain path and its RX fill path. To mutate the internal
/// smoltcp interface + queues through a shared reference we need
/// **interior mutability** — standard idiom: wrap the mutable state
/// in a `Mutex<T>`.
#[derive(Debug)]
pub struct SmoltcpBackend {
    inner: Mutex<State>,
}

/// All mutable state the backend holds — the smoltcp interface and
/// its backing phy device.
///
/// Kept behind one `Mutex` so there's no lock-ordering concern
/// between device access and interface polling (both are driven
/// together in `poll()`).
struct State {
    /// The underlying smoltcp phy device. Owns the frame queues
    /// (`guest_to_stack` = inbound from guest, `stack_to_guest` =
    /// outbound to guest).
    device: SmoltDevice,
    /// smoltcp's protocol-engine side. Holds the ARP cache, routing
    /// table, IP configuration, and will hold the TCP socket set
    /// starting with commit #3.
    iface: Interface,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("device", &self.device)
            .field("iface", &"<smoltcp::iface::Interface>")
            .finish()
    }
}

impl State {
    /// Drive the smoltcp interface forward: let it consume any
    /// inbound frames the guest produced and emit any outbound
    /// frames it has queued. Called from both `read_frame` and
    /// `write_frame` so forward progress always happens.
    ///
    /// # Rust concept: split borrow of struct fields
    ///
    /// `iface.poll(now, &mut device, &mut sockets)` wants two
    /// mutable references at the same time. Rust usually forbids
    /// this through a method (`self.iface.poll(..., &mut self.device, ...)`
    /// would try to borrow `self` twice). But Rust **does** allow
    /// two mutable refs to **different fields** of the same struct —
    /// a split borrow. We access `self.iface` and `self.device`
    /// separately as distinct struct fields, and the compiler
    /// tracks that they don't alias.
    fn poll(&mut self) {
        // Empty socket set for commit #2 — no TCP sockets yet.
        // The interface will still handle ARP + ICMP out of the box
        // because those are built into the protocol engine, not
        // delivered via sockets.
        let mut sockets = SocketSet::new(vec![]);
        let now = Instant::from_millis(0); // monotonic ok for v1
        self.iface.poll(now, &mut self.device, &mut sockets);
    }
}

impl SmoltcpBackend {
    /// Build a backend with the default IPv4 (`169.254.0.1`) and
    /// MAC (`52:54:00:ff:ff:01`). Commit #3+ adds knob to override
    /// these for multi-tenant deployments.
    pub fn new() -> Self {
        let mut device = SmoltDevice::new(DEVICE_MTU);

        // Build the smoltcp interface. The config carries the MAC;
        // IP address is set separately via `update_ip_addrs`.
        let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(DEFAULT_HOST_MAC)));
        let mut iface = Interface::new(config, &mut device, Instant::from_millis(0));
        iface.update_ip_addrs(|addrs| {
            // `.push` returns Err if the heapless vector is full,
            // but we only push one address on a fresh interface so
            // this is infallible in practice. Keep the result
            // handling here as a safety net.
            let addr = IpCidr::new(
                IpAddress::v4(
                    DEFAULT_HOST_IPV4[0],
                    DEFAULT_HOST_IPV4[1],
                    DEFAULT_HOST_IPV4[2],
                    DEFAULT_HOST_IPV4[3],
                ),
                HOST_IPV4_PREFIX_LEN,
            );
            let _ = addrs.push(addr);
        });

        // Default route pointing at the guest-side IP. For a /16
        // link-local setup both sides are "on-link" so technically
        // no gateway is needed, but setting one makes
        // `arp who-has 169.254.0.1` resolve through the route
        // instead of broadcast.
        let gateway = Ipv4Address::new(
            DEFAULT_GUEST_IPV4[0],
            DEFAULT_GUEST_IPV4[1],
            DEFAULT_GUEST_IPV4[2],
            DEFAULT_GUEST_IPV4[3],
        );
        let _ = iface.routes_mut().add_default_ipv4_route(gateway);

        Self {
            inner: Mutex::new(State { device, iface }),
        }
    }

    /// Push a frame directly into the outbound-to-guest queue,
    /// bypassing smoltcp. Test helper only — real production path
    /// goes through the stack via smoltcp's `transmit`.
    #[cfg(test)]
    pub(crate) fn inject_outbound_frame_for_test(&self, frame: Vec<u8>) {
        self.inner
            .lock()
            .expect("smoltcp backend state mutex poisoned")
            .device
            .stack_to_guest
            .push_back(frame);
    }

    /// Depths of the two internal queues `(guest→stack, stack→guest)`.
    /// Test helper for asserting on queue state.
    #[cfg(test)]
    pub(crate) fn queue_depths(&self) -> (usize, usize) {
        let s = self
            .inner
            .lock()
            .expect("smoltcp backend state mutex poisoned");
        (s.device.guest_to_stack.len(), s.device.stack_to_guest.len())
    }
}

impl Default for SmoltcpBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkBackend for SmoltcpBackend {
    /// Pop one outbound frame from the stack (if any), copy into
    /// `buf`. Polls the interface first so smoltcp has a chance to
    /// generate frames in response to internal events (ARP replies
    /// queued for retransmission, TCP ACKs, …).
    ///
    /// Semantics mirror [`crate::MockBackend::read_frame`] +
    /// Linux TAP: if `buf` is smaller than the next frame we copy
    /// as much as fits and drop the rest. The virtio-net RX path
    /// always sizes buffers at ≥ MTU, so truncation only happens
    /// in tests.
    fn read_frame(&self, buf: &mut [u8]) -> Result<usize> {
        let mut state = self
            .inner
            .lock()
            .expect("smoltcp backend state mutex poisoned");
        state.poll();
        match state.device.stack_to_guest.pop_front() {
            None => Ok(0),
            Some(frame) => {
                let n = frame.len().min(buf.len());
                buf[..n].copy_from_slice(&frame[..n]);
                Ok(n)
            }
        }
    }

    /// Hand one guest-produced frame to the stack. Then poll so
    /// smoltcp parses it, updates ARP cache / socket state, and
    /// potentially queues a reply frame (which `read_frame` will
    /// pop on the next call from the virtio RX fill path).
    fn write_frame(&self, frame: &[u8]) -> Result<()> {
        let mut state = self
            .inner
            .lock()
            .expect("smoltcp backend state mutex poisoned");
        state.device.guest_to_stack.push_back(frame.to_vec());
        state.poll();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SmoltDevice — our `smoltcp::phy::Device` implementation
// ---------------------------------------------------------------------------

/// Phy device backing the smoltcp Interface. Owns two frame queues:
///
/// - `guest_to_stack` — frames the guest sent us (via
///   `NetworkBackend::write_frame`). smoltcp's `receive()` pops
///   these when it wants to parse an inbound frame.
/// - `stack_to_guest` — frames smoltcp wants to deliver to the guest.
///   smoltcp's `transmit()` pushes here; `NetworkBackend::read_frame`
///   pops them for the virtio-net RX fill path.
///
/// Naming reflects "which direction, from this device's perspective":
/// from smoltcp's POV, "receive" means "inbound from the wire", i.e.
/// from the guest.
#[derive(Debug)]
struct SmoltDevice {
    guest_to_stack: VecDeque<Vec<u8>>,
    stack_to_guest: VecDeque<Vec<u8>>,
    mtu: usize,
}

impl SmoltDevice {
    fn new(mtu: usize) -> Self {
        Self {
            guest_to_stack: VecDeque::new(),
            stack_to_guest: VecDeque::new(),
            mtu,
        }
    }
}

impl Device for SmoltDevice {
    // # Rust concept: Generic Associated Type (GAT)
    //
    // `type RxToken<'a>` is a GAT — the associated type is itself
    // parameterised by a lifetime. The lifetime `'a` is bound to
    // the `&mut self` call that produced the token. Our concrete
    // tokens below ignore the lifetime (RxToken owns its frame,
    // TxToken captures `'a` only to hold a `&mut VecDeque`), but
    // the trait signature forces us to declare them as generic.
    //
    // In Java/Kotlin there's no equivalent — Java generics have no
    // notion of lifetime. GATs are a specifically-Rust pattern that
    // lets a trait express "the returned handle borrows from `&mut self`
    // and cannot outlive it".
    type RxToken<'a>
        = SmoltRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = SmoltTxToken<'a>
    where
        Self: 'a;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = self.mtu;
        caps.medium = Medium::Ethernet;
        caps
    }

    fn receive(&mut self, _: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Only produce a token if the guest has actually given us a
        // frame to consume. Returning `None` tells smoltcp "nothing
        // new to parse, don't call me back right now".
        let frame = self.guest_to_stack.pop_front()?;
        let rx = SmoltRxToken { frame };
        let tx = SmoltTxToken {
            stack_to_guest: &mut self.stack_to_guest,
            mtu: self.mtu,
        };
        Some((rx, tx))
    }

    fn transmit(&mut self, _: Instant) -> Option<Self::TxToken<'_>> {
        // Always have transmit capacity in a `VecDeque` — bounded
        // only by host RAM. Real NICs sometimes return None to
        // apply backpressure; our userspace stack doesn't need
        // that for v1.
        Some(SmoltTxToken {
            stack_to_guest: &mut self.stack_to_guest,
            mtu: self.mtu,
        })
    }
}

/// RxToken implementation. Owns the inbound frame as `Vec<u8>` so no
/// lifetime parameter is needed — smoltcp's trait requires the GAT
/// `<'a>` but we don't actually borrow anything for RX.
pub(crate) struct SmoltRxToken {
    frame: Vec<u8>,
}

impl RxToken for SmoltRxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut self.frame)
    }
}

/// TxToken implementation. Borrows `&'a mut VecDeque<Vec<u8>>` from
/// the device so that `consume` can push the newly-built frame
/// without any extra plumbing.
pub(crate) struct SmoltTxToken<'a> {
    stack_to_guest: &'a mut VecDeque<Vec<u8>>,
    mtu: usize,
}

impl<'a> TxToken for SmoltTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // smoltcp asks for a `len`-byte scratch buffer, fills it,
        // then this closure returns. We allocate a `Vec<u8>` of
        // exactly `len`, hand it to the closure, then push onto
        // the outbound queue. Clamp to MTU as a safety net —
        // smoltcp's own `max_transmission_unit` cap should mean we
        // never see len > mtu.
        let len = len.min(self.mtu);
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.stack_to_guest.push_back(buf);
        r
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
    fn read_frame_returns_zero_when_empty() {
        let bk = SmoltcpBackend::new();
        let mut buf = [0u8; 64];
        assert_eq!(bk.read_frame(&mut buf).unwrap(), 0);
    }

    #[test]
    fn write_frame_feeds_stack_and_poll_consumes_it() {
        // A malformed 64-byte "frame" that smoltcp can't parse is
        // still accepted + consumed by poll (smoltcp silently drops
        // frames it can't decode). The inbound queue should end up
        // empty after one write.
        let bk = SmoltcpBackend::new();
        bk.write_frame(&[0u8; 64]).unwrap();
        // guest_to_stack drained by poll, stack_to_guest may or may
        // not have an outbound (no, because smoltcp dropped the
        // garbage without a reply).
        let (g2s, _s2g) = bk.queue_depths();
        assert_eq!(g2s, 0, "inbound queue must be drained by poll");
    }

    #[test]
    fn inject_outbound_frame_round_trips_via_read_frame() {
        // Pre-populate the outbound queue (bypassing the stack) so
        // we can verify the read_frame path works end-to-end
        // independently of smoltcp's own frame production.
        let bk = SmoltcpBackend::new();
        let injected = (0..80u8).collect::<Vec<_>>();
        bk.inject_outbound_frame_for_test(injected.clone());

        let mut buf = [0u8; 2048];
        let n = bk.read_frame(&mut buf).unwrap();
        assert_eq!(n, 80);
        assert_eq!(&buf[..n], &injected[..]);
    }

    #[test]
    fn oversized_frame_is_truncated_like_tap_and_mock() {
        let bk = SmoltcpBackend::new();
        bk.inject_outbound_frame_for_test(vec![0xAB; 1500]);

        let mut small = [0u8; 64];
        let n = bk.read_frame(&mut small).unwrap();
        assert_eq!(n, 64);
        assert_eq!(small, [0xAB; 64]);

        // Frame consumed on truncating read.
        let mut big = [0u8; 2048];
        assert_eq!(bk.read_frame(&mut big).unwrap(), 0);
    }

    #[test]
    fn arp_request_from_guest_gets_a_reply() {
        // Build a minimal ARP who-has request from the guest side
        // asking "who has 169.254.0.1 (our host IP)?" and feed it
        // via write_frame. Then read_frame should give us an ARP
        // reply from smoltcp with our host MAC.
        //
        // Ethernet frame (14 bytes) + ARP packet (28 bytes) = 42 bytes.
        let mut req = Vec::with_capacity(42);
        // Ethernet header
        req.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]); // dst = broadcast
        req.extend_from_slice(&[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]); // src = guest MAC
        req.extend_from_slice(&[0x08, 0x06]); // ethertype = ARP
                                              // ARP header
        req.extend_from_slice(&[0x00, 0x01]); // hw type = Ethernet
        req.extend_from_slice(&[0x08, 0x00]); // proto type = IPv4
        req.extend_from_slice(&[0x06]); // hw addr len = 6
        req.extend_from_slice(&[0x04]); // proto addr len = 4
        req.extend_from_slice(&[0x00, 0x01]); // operation = request
        req.extend_from_slice(&[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]); // sender hw addr
        req.extend_from_slice(&DEFAULT_GUEST_IPV4); // sender proto addr
        req.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // target hw addr (zero in request)
        req.extend_from_slice(&DEFAULT_HOST_IPV4); // target proto addr

        let bk = SmoltcpBackend::new();
        bk.write_frame(&req).unwrap();

        // The ARP reply, if any, is now sitting in stack_to_guest.
        let mut buf = [0u8; 2048];
        let n = bk.read_frame(&mut buf).unwrap();

        assert!(
            n > 0,
            "smoltcp should have produced an ARP reply, but read_frame returned 0"
        );
        // Verify it's an ARP reply (ethertype 0x0806 at offset 12,
        // operation 0x0002 at offset 20).
        assert_eq!(&buf[12..14], &[0x08, 0x06], "reply ethertype must be ARP");
        assert_eq!(
            &buf[20..22],
            &[0x00, 0x02],
            "reply op must be ARP reply (0x0002), got {:?}",
            &buf[20..22]
        );
        // The reply's sender hw addr (offset 22..28) should be our
        // configured host MAC.
        assert_eq!(
            &buf[22..28],
            &DEFAULT_HOST_MAC,
            "reply sender MAC must be our host MAC"
        );
    }

    #[test]
    fn default_ipv4_constants_are_in_link_local_range() {
        assert_eq!(DEFAULT_HOST_IPV4[0..2], [169, 254]);
        assert_eq!(DEFAULT_GUEST_IPV4[0..2], [169, 254]);
        assert_ne!(DEFAULT_HOST_IPV4, DEFAULT_GUEST_IPV4);
    }
}
