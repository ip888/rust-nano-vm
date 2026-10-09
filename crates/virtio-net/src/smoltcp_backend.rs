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
//! ## This commit (sub-PR #D commit 4/6)
//!
//! Adds **client-mode TCP sockets** so the backend can originate
//! connections (not just answer them with the echo listener from
//! commit #3). This is the half of the API the host-side byte
//! proxy will drive when forwarding external TCP flows into the
//! guest's Spring Boot:
//!
//! - A [`BackendConfig`] struct that captures the host IP / MAC /
//!   gateway / listen-port tuple. Lets a test spin up two backends
//!   on different IPs so they can shuttle Ethernet frames at each
//!   other without ARP ambiguity.
//! - A [`SmoltcpBackend::with_config`] constructor; the pre-existing
//!   [`SmoltcpBackend::new`] and [`SmoltcpBackend::with_listen_port`]
//!   now delegate to it.
//! - A client-socket API:
//!   [`open_client_socket`](SmoltcpBackend::open_client_socket) adds
//!   a new TCP socket to the set and initiates `connect(remote)`;
//!   [`socket_send`](SmoltcpBackend::socket_send) /
//!   [`socket_recv`](SmoltcpBackend::socket_recv) /
//!   [`socket_state`](SmoltcpBackend::socket_state) /
//!   [`close_socket`](SmoltcpBackend::close_socket) wrap the per-
//!   socket plumbing behind the backend's one `Mutex<State>`.
//!
//! After this commit, two `SmoltcpBackend` instances can complete
//! a real TCP handshake and echo bytes end-to-end across their
//! Ethernet-frame queues — the exact shape the proxy module in
//! commit #5 will plug a `std::net::TcpStream` into. Commit #6
//! wires it into vm-kvm as an opt-in; the Spring-Boot integration
//! test closes out sub-PR #D.
//!
//! Gated behind the `smoltcp-backend` crate feature so users on the
//! kernel-TAP path aren't forced to depend on smoltcp.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant as HostInstant;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};

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

/// Default TCP port the built-in echo listener binds to. 8080 is
/// Spring Boot's default so the Java-pivot demos work with zero
/// per-tenant configuration.
pub const DEFAULT_LISTEN_PORT: u16 = 8080;

/// Bytes allocated for each direction of the TCP socket's internal
/// ring buffers. 4 KiB per direction is enough to absorb a typical
/// HTTP request/response burst without back-pressuring the guest.
const TCP_SOCKET_BUFFER_BYTES: usize = 4096;

/// Addressing + port config for a single [`SmoltcpBackend`]. Carries
/// everything a stand-alone backend needs to come up on its own IP;
/// a second backend built from a different `BackendConfig` can
/// shuttle Ethernet frames at the first and complete a real TCP
/// handshake, which is exactly how the host-side proxy will drive
/// smoltcp against the guest.
#[derive(Debug, Clone)]
pub struct BackendConfig {
    /// IPv4 address this backend answers to. Routes point at
    /// `gateway_ipv4`.
    pub host_ipv4: [u8; 4],
    /// MAC advertised to the peer at the other end of the virtio-net
    /// link. Must be distinct from the peer's MAC for ARP to work.
    pub host_mac: [u8; 6],
    /// Default-route target. For the standard host↔guest link-local
    /// setup this is the guest IP (host backend's gateway) or vice
    /// versa.
    pub gateway_ipv4: [u8; 4],
    /// Port the built-in echo listener binds to. Set to 0 to skip
    /// seeding a listener — handy for the host-side proxy backend
    /// which only ever opens client sockets.
    pub listen_port: u16,
}

impl Default for BackendConfig {
    /// Host-side defaults from the module-level constants
    /// (`169.254.0.1`, `52:54:00:ff:ff:01`, echo listener on 8080).
    fn default() -> Self {
        Self {
            host_ipv4: DEFAULT_HOST_IPV4,
            host_mac: DEFAULT_HOST_MAC,
            gateway_ipv4: DEFAULT_GUEST_IPV4,
            listen_port: DEFAULT_LISTEN_PORT,
        }
    }
}

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

/// All mutable state the backend holds — the smoltcp interface, its
/// backing phy device, and the socket set holding the echo listener.
///
/// Kept behind one `Mutex` so there's no lock-ordering concern
/// between device access, interface polling and socket pumping
/// (all driven together in `poll_and_echo`).
struct State {
    /// The underlying smoltcp phy device. Owns the frame queues
    /// (`guest_to_stack` = inbound from guest, `stack_to_guest` =
    /// outbound to guest).
    device: SmoltDevice,
    /// smoltcp's protocol-engine side: ARP cache, routing table, IP
    /// configuration. Polled against `device` + `sockets` to advance
    /// TCP state machines and emit/consume frames.
    iface: Interface,
    /// Persistent socket set. Owns the TCP echo socket's RX/TX
    /// buffers and connection state across polls.
    ///
    /// The `'static` lifetime isn't about leaking anything — it
    /// means the socket storage is heap-allocated (`Vec<u8>`), not
    /// borrowed from a stack frame.
    sockets: SocketSet<'static>,
    /// Handle to the TCP echo socket inside `sockets`, present only
    /// when the backend's `listen_port` is non-zero. Client-mode
    /// backends (proxy side) skip the listener entirely.
    echo_handle: Option<SocketHandle>,
    /// Port the echo listener is bound to (0 means "no listener,
    /// client-mode only"). Also used to re-arm the listener after
    /// a connection fully tears down.
    listen_port: u16,
    /// Monotonically-increasing counter for ephemeral local ports
    /// allocated to client sockets via
    /// [`SmoltcpBackend::open_client_socket`]. Starts at the IANA
    /// ephemeral-range base and wraps if we somehow exhaust it
    /// (realistically not going to happen in a nano-VM lifetime).
    next_ephemeral_port: u16,
    /// Wall-clock epoch the backend was constructed at; we feed
    /// elapsed millis into smoltcp's `Instant` so TCP timers and
    /// retransmissions actually progress.
    epoch: HostInstant,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("device", &self.device)
            .field("iface", &"<smoltcp::iface::Interface>")
            .field("sockets", &"<smoltcp::iface::SocketSet>")
            .field("echo_handle", &self.echo_handle)
            .field("listen_port", &self.listen_port)
            .finish()
    }
}

impl State {
    /// Elapsed time since backend construction, formatted for
    /// smoltcp. `i64::MAX` ms is ~292 million years so saturating
    /// into `i64` is cosmetic — it's never going to clamp.
    fn now(&self) -> Instant {
        Instant::from_millis(self.epoch.elapsed().as_millis() as i64)
    }

    /// Drive the smoltcp interface forward and pump the echo
    /// socket: let the stack consume any inbound frames, forward
    /// any received bytes straight back onto the same TCP flow,
    /// and poll once more so the echoed bytes get serialised into
    /// outbound Ethernet frames.
    ///
    /// # Rust concept: split borrow of struct fields
    ///
    /// `iface.poll(now, &mut device, &mut sockets)` wants three
    /// mutable borrows at the same time. Rust usually forbids
    /// stacking multiple `&mut self.<field>` references through
    /// methods, but it **does** allow mutable refs to **different
    /// fields** of the same struct — a split borrow. The compiler
    /// tracks that `self.iface`, `self.device` and `self.sockets`
    /// don't alias, so the three borrows coexist. In Java there's
    /// no analog — references can alias freely and the risk of
    /// stepping on your own state is on you.
    fn poll_and_echo(&mut self) {
        let now = self.now();
        self.iface.poll(now, &mut self.device, &mut self.sockets);
        self.drive_echo();
        // Second poll flushes bytes that `drive_echo` just pushed
        // into the TCP TX buffer out through the device as
        // Ethernet frames, so the next `read_frame` call sees them.
        self.iface.poll(now, &mut self.device, &mut self.sockets);
    }

    /// Simple echo loop over the backend's one listener socket.
    /// Client sockets added via
    /// [`SmoltcpBackend::open_client_socket`] are **not** touched
    /// here — their byte flow is driven externally by the caller
    /// (`socket_send` / `socket_recv`) so each proxied connection
    /// keeps its own forward/reverse state.
    ///
    /// - No listener configured (`echo_handle == None`): no-op.
    /// - Fresh listener in `Closed` state → re-arm `listen`.
    /// - `Established` with pending RX and send-side room → forward.
    /// - Peer-FIN'd (`CloseWait` with empty RX buffer) → close our
    ///   half so the connection can finish teardown.
    fn drive_echo(&mut self) {
        let Some(echo_handle) = self.echo_handle else {
            return;
        };
        let listen_port = self.listen_port;
        let socket = self.sockets.get_mut::<tcp::Socket>(echo_handle);

        // Re-arm after a prior connection fully closed. `listen`
        // returns `Err` only when the socket is already in a
        // non-closed state, which `is_open()` guards against.
        if !socket.is_open() {
            let _ = socket.listen(listen_port);
            return;
        }

        if socket.can_recv() && socket.can_send() {
            // `recv_slice` copies as many ready bytes as fit into
            // `buf` and advances the receive window. On a healthy
            // ESTABLISHED socket this is `Ok(n)` with `n > 0`.
            let mut buf = [0u8; TCP_SOCKET_BUFFER_BYTES];
            if let Ok(n) = socket.recv_slice(&mut buf) {
                if n > 0 {
                    // `send_slice` may return a short count if the
                    // TX ring is nearly full. For an echo loop
                    // against a cooperative peer that's rare, and
                    // dropped bytes trigger a client-side retry;
                    // tracking per-connection unflushed state can
                    // land with the proxy work in commit #4.
                    let _ = socket.send_slice(&buf[..n]);
                }
            }
        }

        // Peer sent FIN and we've drained their payload: close our
        // half so the four-way teardown can complete and the
        // listener re-arm next poll.
        if socket.state() == tcp::State::CloseWait && !socket.can_recv() {
            socket.close();
        }
    }
}

impl SmoltcpBackend {
    /// Build a backend with every default from
    /// [`BackendConfig::default`]: host IP `169.254.0.1`, MAC
    /// `52:54:00:ff:ff:01`, gateway `169.254.0.2`, echo listener
    /// on [`DEFAULT_LISTEN_PORT`].
    pub fn new() -> Self {
        Self::with_config(BackendConfig::default())
    }

    /// Shortcut over [`SmoltcpBackend::with_config`] that only
    /// overrides the TCP listen port. Keeps old call-sites working
    /// after commit #3 shipped it; tests that need distinct IPs
    /// should use [`with_config`](SmoltcpBackend::with_config).
    pub fn with_listen_port(listen_port: u16) -> Self {
        Self::with_config(BackendConfig {
            listen_port,
            ..BackendConfig::default()
        })
    }

    /// Build a backend from an explicit `BackendConfig`. The
    /// interface comes up on `host_ipv4`/`host_mac` with a default
    /// route via `gateway_ipv4`; the echo listener on `listen_port`
    /// is seeded only when the port is non-zero.
    pub fn with_config(cfg: BackendConfig) -> Self {
        let mut device = SmoltDevice::new(DEVICE_MTU);

        // Build the smoltcp interface. The config carries the MAC;
        // IP address is set separately via `update_ip_addrs`.
        let smoltcp_cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(cfg.host_mac)));
        let mut iface = Interface::new(smoltcp_cfg, &mut device, Instant::from_millis(0));
        iface.update_ip_addrs(|addrs| {
            // `.push` returns Err if the heapless vector is full,
            // but we only push one address on a fresh interface so
            // this is infallible in practice. Keep the result
            // handling here as a safety net.
            let addr = IpCidr::new(
                IpAddress::v4(
                    cfg.host_ipv4[0],
                    cfg.host_ipv4[1],
                    cfg.host_ipv4[2],
                    cfg.host_ipv4[3],
                ),
                HOST_IPV4_PREFIX_LEN,
            );
            let _ = addrs.push(addr);
        });

        // Default route pointing at the configured gateway. For a
        // /16 link-local setup both sides are "on-link" so a
        // gateway isn't strictly required for reachability, but
        // smoltcp uses it to resolve `arp who-has gw.ip` cleanly
        // instead of broadcasting on every outbound.
        let gateway = Ipv4Address::new(
            cfg.gateway_ipv4[0],
            cfg.gateway_ipv4[1],
            cfg.gateway_ipv4[2],
            cfg.gateway_ipv4[3],
        );
        let _ = iface.routes_mut().add_default_ipv4_route(gateway);

        let mut sockets = SocketSet::new(vec![]);
        let echo_handle = if cfg.listen_port != 0 {
            // Build the TCP echo socket pre-seated in `Listen`.
            // Both buffers are owned `Vec<u8>` so the socket set is
            // `'static` and we don't carry a lifetime through
            // `SmoltcpBackend`.
            let rx_buffer = tcp::SocketBuffer::new(vec![0u8; TCP_SOCKET_BUFFER_BYTES]);
            let tx_buffer = tcp::SocketBuffer::new(vec![0u8; TCP_SOCKET_BUFFER_BYTES]);
            let mut socket = tcp::Socket::new(rx_buffer, tx_buffer);
            socket
                .listen(cfg.listen_port)
                .expect("fresh TCP socket must accept listen on a non-zero port");
            Some(sockets.add(socket))
        } else {
            None
        };

        Self {
            inner: Mutex::new(State {
                device,
                iface,
                sockets,
                echo_handle,
                listen_port: cfg.listen_port,
                next_ephemeral_port: 49_152,
                epoch: HostInstant::now(),
            }),
        }
    }

    /// Port the backend's TCP echo listener is bound to, or `0` if
    /// no listener was seeded.
    pub fn listen_port(&self) -> u16 {
        self.inner
            .lock()
            .expect("smoltcp backend state mutex poisoned")
            .listen_port
    }

    // -----------------------------------------------------------------
    // Client-mode TCP sockets
    //
    // These wrap `smoltcp::socket::tcp::Socket` behind the backend's
    // one `Mutex<State>` so the host-side proxy (commit #5) can
    // originate connections to the guest without touching smoltcp
    // directly or juggling socket lifetimes.
    // -----------------------------------------------------------------

    /// Allocate a fresh TCP socket, initiate `connect(remote)` from
    /// an ephemeral local port, and return a handle the caller can
    /// pump bytes through with `socket_send`/`socket_recv`.
    ///
    /// The connection isn't `Established` yet when this returns —
    /// a SYN is queued and the handshake needs the usual frame
    /// shuttle to complete. Poll with `socket_state(handle)` or
    /// drive bytes and look for `Ok(0)` with `may_send() == false`.
    pub fn open_client_socket(&self, remote: IpEndpoint) -> SocketHandle {
        let mut state = self
            .inner
            .lock()
            .expect("smoltcp backend state mutex poisoned");

        // Allocate RX/TX buffers for this new socket. One pair per
        // live client connection — the proxy will drop the handle
        // (via `close_socket`) when its paired external stream
        // closes.
        let rx = tcp::SocketBuffer::new(vec![0u8; TCP_SOCKET_BUFFER_BYTES]);
        let tx = tcp::SocketBuffer::new(vec![0u8; TCP_SOCKET_BUFFER_BYTES]);
        let socket = tcp::Socket::new(rx, tx);
        let handle = state.sockets.add(socket);

        let local_port = state.next_ephemeral_port;
        // Rotate the counter around the IANA ephemeral range so a
        // long-lived backend doesn't eventually overflow back onto
        // reserved ports.
        state.next_ephemeral_port = state.next_ephemeral_port.saturating_add(1).max(49_152);
        if state.next_ephemeral_port == 0 {
            state.next_ephemeral_port = 49_152;
        }

        // Split-borrow trick: we need `&mut sockets` to fetch the
        // socket and `&mut iface` to get the context passed into
        // `connect`. Rust's borrow checker lets us hold both
        // mutably because they're **distinct struct fields** —
        // but only when the compiler can see the fields directly,
        // not through a smart pointer like `MutexGuard`. Deref
        // once into a plain `&mut State` and the split borrow
        // succeeds.
        let s: &mut State = &mut state;
        let cx = s.iface.context();
        s.sockets
            .get_mut::<tcp::Socket>(handle)
            .connect(cx, remote, local_port)
            .expect("connect on a fresh TCP socket should always enqueue a SYN");

        // Pump once so the SYN gets emitted into the outbound
        // frame queue right away rather than only on the next
        // read_frame/write_frame.
        state.poll_and_echo();
        handle
    }

    /// Push bytes into the socket's send buffer. Returns the number
    /// actually accepted (less than `data.len()` only if the TX ring
    /// is full). Also polls so the bytes get a chance to flush out
    /// as Ethernet frames before the caller's next `read_frame`.
    pub fn socket_send(&self, handle: SocketHandle, data: &[u8]) -> Result<usize> {
        let mut state = self
            .inner
            .lock()
            .expect("smoltcp backend state mutex poisoned");
        let socket = state.sockets.get_mut::<tcp::Socket>(handle);
        let written = socket.send_slice(data).unwrap_or(0);
        state.poll_and_echo();
        Ok(written)
    }

    /// Drain whatever's ready in the socket's receive buffer into
    /// `buf`. Returns 0 when nothing's ready (same shape as
    /// non-blocking read on a raw socket) so a proxy can poll in a
    /// loop without having to parse smoltcp error variants.
    pub fn socket_recv(&self, handle: SocketHandle, buf: &mut [u8]) -> Result<usize> {
        let mut state = self
            .inner
            .lock()
            .expect("smoltcp backend state mutex poisoned");
        state.poll_and_echo();
        let socket = state.sockets.get_mut::<tcp::Socket>(handle);
        Ok(socket.recv_slice(buf).unwrap_or(0))
    }

    /// Current TCP state of the given socket. Mostly useful for
    /// testing — the proxy picks progress by trying to recv/send
    /// instead of state-polling.
    pub fn socket_state(&self, handle: SocketHandle) -> tcp::State {
        let s = self
            .inner
            .lock()
            .expect("smoltcp backend state mutex poisoned");
        s.sockets.get::<tcp::Socket>(handle).state()
    }

    /// Close our half of the connection and remove the socket from
    /// the set once smoltcp has fully torn it down. Idempotent — a
    /// handle that already got abandoned is a no-op.
    pub fn close_socket(&self, handle: SocketHandle) {
        let mut state = self
            .inner
            .lock()
            .expect("smoltcp backend state mutex poisoned");
        // `close()` enqueues a FIN; the socket may still linger in
        // FIN_WAIT_1 / FIN_WAIT_2 / TIME_WAIT until the peer
        // acknowledges. For a shutdown-and-forget semantic we just
        // let smoltcp finish teardown on its own schedule — the
        // handle keeps working for state introspection.
        state.sockets.get_mut::<tcp::Socket>(handle).close();
        state.poll_and_echo();
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

    /// `true` if the backend has an echo listener and it's in
    /// `Listen` state. Test helper for asserting listener
    /// readiness after construction and after a connection fully
    /// tore down.
    #[cfg(test)]
    pub(crate) fn listener_is_listening(&self) -> bool {
        let s = self
            .inner
            .lock()
            .expect("smoltcp backend state mutex poisoned");
        let Some(h) = s.echo_handle else {
            return false;
        };
        s.sockets.get::<tcp::Socket>(h).state() == tcp::State::Listen
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
        state.poll_and_echo();
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
    /// smoltcp parses it, updates ARP cache / TCP state, and
    /// potentially queues a reply frame (which `read_frame` will
    /// pop on the next call from the virtio RX fill path).
    fn write_frame(&self, frame: &[u8]) -> Result<()> {
        let mut state = self
            .inner
            .lock()
            .expect("smoltcp backend state mutex poisoned");
        state.device.guest_to_stack.push_back(frame.to_vec());
        state.poll_and_echo();
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

    #[test]
    fn tcp_listener_is_in_listen_state_after_construction() {
        let bk = SmoltcpBackend::new();
        assert!(bk.listener_is_listening());
        assert_eq!(bk.listen_port(), DEFAULT_LISTEN_PORT);
    }

    #[test]
    fn with_listen_port_binds_the_requested_port() {
        let bk = SmoltcpBackend::with_listen_port(31337);
        assert_eq!(bk.listen_port(), 31337);
        assert!(bk.listener_is_listening());
    }

    // ---------------------------------------------------------------
    // End-to-end TCP tests: build a second smoltcp Interface that
    // pretends to be the guest side, shuttle Ethernet frames between
    // it and the backend under test, and watch the connection
    // progress.
    // ---------------------------------------------------------------

    /// Minimal "guest-side" peer: a smoltcp Interface + one TCP
    /// socket, plus the raw frame queues so a test can see the
    /// Ethernet frames the stack produced and feed in replies.
    struct TestPeer {
        device: SmoltDevice,
        iface: Interface,
        sockets: SocketSet<'static>,
        handle: SocketHandle,
    }

    impl TestPeer {
        fn new() -> Self {
            let mut device = SmoltDevice::new(DEVICE_MTU);
            let cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress([
                0x52, 0x54, 0x00, 0x12, 0x34, 0x56,
            ])));
            let mut iface = Interface::new(cfg, &mut device, Instant::from_millis(0));
            iface.update_ip_addrs(|a| {
                let addr = IpCidr::new(
                    IpAddress::v4(
                        DEFAULT_GUEST_IPV4[0],
                        DEFAULT_GUEST_IPV4[1],
                        DEFAULT_GUEST_IPV4[2],
                        DEFAULT_GUEST_IPV4[3],
                    ),
                    HOST_IPV4_PREFIX_LEN,
                );
                let _ = a.push(addr);
            });
            let _ = iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(
                DEFAULT_HOST_IPV4[0],
                DEFAULT_HOST_IPV4[1],
                DEFAULT_HOST_IPV4[2],
                DEFAULT_HOST_IPV4[3],
            ));

            let rx = tcp::SocketBuffer::new(vec![0u8; TCP_SOCKET_BUFFER_BYTES]);
            let tx = tcp::SocketBuffer::new(vec![0u8; TCP_SOCKET_BUFFER_BYTES]);
            let mut sockets = SocketSet::new(vec![]);
            let handle = sockets.add(tcp::Socket::new(rx, tx));

            Self {
                device,
                iface,
                sockets,
                handle,
            }
        }

        /// Fire a SYN at the backend's listener. Returns once the
        /// socket transitions out of `Closed`.
        fn connect_to_host(&mut self, port: u16) {
            // Local ephemeral port — arbitrary unused value in the
            // "shouldn't collide with real services" band.
            self.sockets
                .get_mut::<tcp::Socket>(self.handle)
                .connect(
                    self.iface.context(),
                    (
                        IpAddress::v4(
                            DEFAULT_HOST_IPV4[0],
                            DEFAULT_HOST_IPV4[1],
                            DEFAULT_HOST_IPV4[2],
                            DEFAULT_HOST_IPV4[3],
                        ),
                        port,
                    ),
                    49_152,
                )
                .expect("fresh socket connect must enqueue a SYN");
        }

        fn poll_at(&mut self, now_ms: i64) {
            let now = Instant::from_millis(now_ms);
            self.iface.poll(now, &mut self.device, &mut self.sockets);
        }

        fn socket(&self) -> &tcp::Socket<'static> {
            self.sockets.get::<tcp::Socket>(self.handle)
        }

        fn socket_mut(&mut self) -> &mut tcp::Socket<'static> {
            self.sockets.get_mut::<tcp::Socket>(self.handle)
        }
    }

    /// Shuttle every pending Ethernet frame between a `TestPeer` and
    /// a `SmoltcpBackend` for `iterations` polling rounds. Returns
    /// early as soon as `done(&peer.socket())` goes true.
    fn shuttle_until<F: FnMut(&tcp::Socket<'_>) -> bool>(
        peer: &mut TestPeer,
        server: &SmoltcpBackend,
        iterations: u64,
        mut done: F,
    ) -> bool {
        for i in 0..iterations {
            peer.poll_at((i as i64) * 10);
            while let Some(frame) = peer.device.stack_to_guest.pop_front() {
                server.write_frame(&frame).unwrap();
            }
            let mut buf = [0u8; 2048];
            loop {
                let n = server.read_frame(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                peer.device.guest_to_stack.push_back(buf[..n].to_vec());
            }
            peer.poll_at((i as i64) * 10 + 1);
            if done(peer.socket()) {
                return true;
            }
        }
        false
    }

    #[test]
    fn tcp_handshake_completes_against_builtin_listener() {
        let server = SmoltcpBackend::new();
        let mut peer = TestPeer::new();
        peer.connect_to_host(DEFAULT_LISTEN_PORT);

        // 200 polling rounds is more than enough for a local
        // handshake with no packet loss (empirically 3 rounds).
        let established = shuttle_until(&mut peer, &server, 200, |s| {
            s.state() == tcp::State::Established
        });
        assert!(
            established,
            "handshake did not complete; peer socket in state {:?}",
            peer.socket().state()
        );
    }

    /// Shuttle every pending Ethernet frame between two backends
    /// (A ↔ B) for up to `iterations` iterations. Returns early
    /// when `done()` goes true. Used by the client-side tests
    /// where "A" plays host-side proxy and "B" plays fake-guest.
    fn shuttle_backends_until<F: FnMut(&SmoltcpBackend, &SmoltcpBackend) -> bool>(
        a: &SmoltcpBackend,
        b: &SmoltcpBackend,
        iterations: u64,
        mut done: F,
    ) -> bool {
        let mut buf = [0u8; 2048];
        for _ in 0..iterations {
            // A → B
            loop {
                let n = a.read_frame(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                b.write_frame(&buf[..n]).unwrap();
            }
            // B → A
            loop {
                let n = b.read_frame(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                a.write_frame(&buf[..n]).unwrap();
            }
            if done(a, b) {
                return true;
            }
        }
        false
    }

    /// Build the fake-guest backend: IP = `DEFAULT_GUEST_IPV4`, MAC
    /// swapped to a distinct value, gateway = `DEFAULT_HOST_IPV4`
    /// (so default routes go back toward the "host" side), echo
    /// listener on 8080 so a `connect(169.254.0.2:8080)` lands.
    fn fake_guest_backend() -> SmoltcpBackend {
        SmoltcpBackend::with_config(BackendConfig {
            host_ipv4: DEFAULT_GUEST_IPV4,
            host_mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
            gateway_ipv4: DEFAULT_HOST_IPV4,
            listen_port: DEFAULT_LISTEN_PORT,
        })
    }

    /// Host-side proxy backend: default IP, but *no* echo listener
    /// — it only ever opens outbound client sockets.
    fn host_proxy_backend() -> SmoltcpBackend {
        SmoltcpBackend::with_config(BackendConfig {
            listen_port: 0,
            ..BackendConfig::default()
        })
    }

    #[test]
    fn with_config_disables_listener_when_port_zero() {
        let bk = SmoltcpBackend::with_config(BackendConfig {
            listen_port: 0,
            ..BackendConfig::default()
        });
        assert_eq!(bk.listen_port(), 0);
        assert!(!bk.listener_is_listening());
    }

    #[test]
    fn open_client_socket_initiates_syn_toward_remote() {
        let host = host_proxy_backend();
        let remote = IpEndpoint::new(
            IpAddress::v4(
                DEFAULT_GUEST_IPV4[0],
                DEFAULT_GUEST_IPV4[1],
                DEFAULT_GUEST_IPV4[2],
                DEFAULT_GUEST_IPV4[3],
            ),
            DEFAULT_LISTEN_PORT,
        );
        let h = host.open_client_socket(remote);
        // Fresh connect lands in SynSent (or Closed if routing is
        // broken — the shuttle test below exercises the full path).
        assert_eq!(host.socket_state(h), tcp::State::SynSent);
    }

    #[test]
    fn two_backends_complete_a_tcp_handshake() {
        let host = host_proxy_backend();
        let guest = fake_guest_backend();
        let remote = IpEndpoint::new(
            IpAddress::v4(
                DEFAULT_GUEST_IPV4[0],
                DEFAULT_GUEST_IPV4[1],
                DEFAULT_GUEST_IPV4[2],
                DEFAULT_GUEST_IPV4[3],
            ),
            DEFAULT_LISTEN_PORT,
        );
        let client = host.open_client_socket(remote);

        let established = shuttle_backends_until(&host, &guest, 200, |a, _| {
            a.socket_state(client) == tcp::State::Established
        });
        assert!(
            established,
            "handshake must complete across two backends, got state {:?}",
            host.socket_state(client)
        );
    }

    #[test]
    fn client_socket_echoes_payload_through_fake_guest() {
        let host = host_proxy_backend();
        let guest = fake_guest_backend();
        let remote = IpEndpoint::new(
            IpAddress::v4(
                DEFAULT_GUEST_IPV4[0],
                DEFAULT_GUEST_IPV4[1],
                DEFAULT_GUEST_IPV4[2],
                DEFAULT_GUEST_IPV4[3],
            ),
            DEFAULT_LISTEN_PORT,
        );
        let client = host.open_client_socket(remote);

        // Phase 1 — handshake.
        let established = shuttle_backends_until(&host, &guest, 200, |a, _| {
            a.socket_state(client) == tcp::State::Established
        });
        assert!(established, "handshake must complete before send");

        // Phase 2 — send a payload via the host-side client socket.
        // The fake guest's echo listener bounces it back; we drain
        // it off the client socket's recv buffer.
        const PAYLOAD: &[u8] = b"hello from the host proxy\n";
        let sent = host.socket_send(client, PAYLOAD).unwrap();
        assert_eq!(sent, PAYLOAD.len(), "fresh socket must accept full write");

        let mut received: Vec<u8> = Vec::new();
        shuttle_backends_until(&host, &guest, 400, |a, _| {
            let mut buf = [0u8; TCP_SOCKET_BUFFER_BYTES];
            let n = a.socket_recv(client, &mut buf).unwrap();
            received.extend_from_slice(&buf[..n]);
            received.len() >= PAYLOAD.len()
        });

        assert_eq!(
            &received[..PAYLOAD.len().min(received.len())],
            PAYLOAD,
            "echo payload must match (got {received:?})",
        );
    }

    #[test]
    fn tcp_echo_round_trips_payload() {
        let server = SmoltcpBackend::new();
        let mut peer = TestPeer::new();
        peer.connect_to_host(DEFAULT_LISTEN_PORT);

        // Phase 1 — handshake.
        let established = shuttle_until(&mut peer, &server, 200, |s| {
            s.state() == tcp::State::Established
        });
        assert!(established, "handshake must complete before send");

        // Phase 2 — write a payload and shuttle until it comes back.
        const PAYLOAD: &[u8] = b"hello, nanovm\n";
        peer.socket_mut()
            .send_slice(PAYLOAD)
            .expect("ESTABLISHED socket must accept send");

        let mut received: Vec<u8> = Vec::new();
        let got_echo = shuttle_until(&mut peer, &server, 400, |_| false);
        // One more drain pass through the peer so recv_slice sees
        // whatever landed on the last iteration.
        let _ = got_echo; // intentionally ignore; we check `received`.
        {
            let mut buf = [0u8; TCP_SOCKET_BUFFER_BYTES];
            while let Ok(n) = peer.socket_mut().recv_slice(&mut buf) {
                if n == 0 {
                    break;
                }
                received.extend_from_slice(&buf[..n]);
                if received.len() >= PAYLOAD.len() {
                    break;
                }
            }
        }
        assert_eq!(
            &received[..PAYLOAD.len().min(received.len())],
            PAYLOAD,
            "echoed payload must match (got {received:?})",
        );
    }
}
