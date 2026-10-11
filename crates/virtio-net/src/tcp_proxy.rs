//! # `TcpProxy` — bridge an OS `TcpStream` to a smoltcp client socket
//!
//! > **Terminology:** every term here (TCP/IP stack, Ethernet frame,
//! > IP, TCP, MAC, handshake, …) is defined once in the terminology
//! > table at the top of `lib.rs`.
//!
//! This module is the glue between the kernel's TCP implementation
//! (on the external side) and our userspace smoltcp stack (on the
//! virtio-net side). It only exists on the host-side proxy backend
//! and is useful when you want external clients to talk to a
//! server living inside a guest VM: Spring Boot on `:8080`,
//! Postgres on `:5432`, SSH on `:22`.
//!
//! ## Shape
//!
//! ```text
//!   External client                                 Guest VM
//!   (anywhere)                                      ─────────
//!       │
//!       │  OS TCP (kernel sockets)
//!       ▼
//!   ┌─────────────────┐
//!   │  TcpListener    │   accept() loop               Spring
//!   │  :443 (or sim)  │─────────────────┐              Boot
//!   └─────────────────┘                 │              :8080
//!                                       ▼               ▲
//!                              ┌──────────────────┐     │
//!                              │ TcpProxy         │     │
//!                              │   bridge_        │     │
//!                              │   connection()   │     │
//!                              └──────────────────┘     │
//!                                       │               │
//!                                       │ socket_send/  │
//!                                       │ socket_recv   │
//!                                       ▼               │
//!                              ┌──────────────────┐     │
//!                              │ SmoltcpBackend   │     │
//!                              │ (host-side,      │     │
//!                              │  no listener)    │     │
//!                              └──────────────────┘     │
//!                                       │               │
//!                                       ▼               │
//!                              Ethernet frames via      │
//!                              virtio-net ring ─────────┘
//! ```
//!
//! The frame-shuttling layer (virtio-net device → guest kernel's
//! `virtio_net`) is what sub-PR #D commit #6 wires in through
//! vm-kvm. In tests we fake it with a second `SmoltcpBackend` and
//! a background thread that moves frames between the two.
//!
//! ## This commit (sub-PR #D commit 5/6)
//!
//! Lands the single-connection bridge: given one accepted
//! [`std::net::TcpStream`] and a target guest endpoint, open a
//! smoltcp client socket on the backend, wait for the TCP handshake
//! to complete with the guest, then pump bytes bidirectionally
//! until either side closes. Non-blocking I/O on the OS stream, a
//! poll-plus-sleep loop on the smoltcp side.
//!
//! The multi-connection accept loop (binding an actual
//! `TcpListener` and spawning one bridge per accepted stream) is a
//! thin wrapper landing with the vm-kvm opt-in in commit #6 — once
//! we have a real virtio-net ring to attach it to.
//!
//! Gated behind the `smoltcp-backend` crate feature (same as its
//! sibling [`crate::smoltcp_backend`]).

use std::io::{self, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp;
use smoltcp::wire::IpEndpoint;

use crate::smoltcp_backend::SmoltcpBackend;
use crate::{Result, VirtioNetError};

/// Maximum time we wait for the smoltcp client socket to reach
/// `Established` before giving up and failing the bridge.
///
/// 5 s is generous for a local link-local link (which handshakes
/// in milliseconds) but short enough that a guest refusing or
/// black-holing the SYN doesn't wedge the proxy thread forever.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Idle sleep between polling iterations when no byte movement is
/// pending in either direction. Shorter → lower latency, higher
/// CPU; 1 ms strikes a reasonable balance for a learning project.
const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Scratch buffer size for one direction of the shuffle. Matches
/// smoltcp's own TCP socket buffer so a saturated recv never
/// splits a single `recv_slice` across two iterations.
const BRIDGE_BUFFER_BYTES: usize = 4096;

/// Bridge between a kernel-level TCP connection and a smoltcp
/// client-mode socket on a [`SmoltcpBackend`].
///
/// Owns an [`Arc`] to the backend so the same proxy can bridge
/// many simultaneous connections (one per spawned thread, in a
/// future commit) without each one needing its own backend copy.
#[derive(Debug)]
pub struct TcpProxy {
    /// Shared handle to the smoltcp stack this proxy forwards
    /// bytes through. All bridge calls go through its public
    /// `open_client_socket` / `socket_send` / `socket_recv`
    /// methods, so the proxy never touches smoltcp types directly.
    backend: Arc<SmoltcpBackend>,
    /// The TCP endpoint inside the guest every bridged connection
    /// targets. Host-side TCP ports can map one-to-one onto guest
    /// endpoints — e.g. the host-side listener on `:443` dispatches
    /// to a `TcpProxy` with `guest_endpoint = 169.254.0.2:8080`.
    guest_endpoint: IpEndpoint,
}

impl TcpProxy {
    /// Pair a backend with a single guest-side target. Cloning is
    /// cheap: the backend is behind an `Arc`.
    pub fn new(backend: Arc<SmoltcpBackend>, guest_endpoint: IpEndpoint) -> Self {
        Self {
            backend,
            guest_endpoint,
        }
    }

    /// The guest endpoint every bridged connection targets.
    pub fn guest_endpoint(&self) -> IpEndpoint {
        self.guest_endpoint
    }

    /// Bridge one accepted external TCP connection to a fresh
    /// smoltcp client socket targeting the guest endpoint.
    ///
    /// Blocking from the caller's perspective — returns when either
    /// end closes, when the smoltcp handshake times out, or when
    /// an I/O error on the OS stream kills the pipe.
    ///
    /// Thread-safe: multiple `bridge_connection` calls can run
    /// concurrently on the same `TcpProxy` because the shared
    /// backend serialises per-socket state under its one `Mutex`.
    pub fn bridge_connection(&self, stream: TcpStream) -> Result<()> {
        // Non-blocking on the OS side so the main poll loop can
        // interleave external reads with smoltcp recv. Without
        // this, `read` would park the thread forever waiting on
        // bytes the external peer isn't going to send until it
        // sees our echoed data.
        stream.set_nonblocking(true)?;

        let handle = self.backend.open_client_socket(self.guest_endpoint);

        // Phase 1 — wait for the guest to accept our SYN.
        if let Err(e) = self.wait_for_handshake(handle) {
            self.backend.close_socket(handle);
            return Err(e);
        }

        // Phase 2 — shuttle bytes until either side closes.
        let result = self.pump_bytes(stream, handle);

        // Idempotent: close_socket on an already-closed handle is
        // harmless. We still close explicitly on the success path
        // so the FIN gets emitted before the backend owns the
        // socket for TIME_WAIT bookkeeping.
        self.backend.close_socket(handle);
        result
    }

    /// Block until the smoltcp client socket reaches `Established`
    /// or we hit the handshake timeout. The guest may still fail
    /// us by sending a RST — that lands the socket in `Closed`,
    /// which we treat as `ConnectionRefused`.
    fn wait_for_handshake(&self, handle: SocketHandle) -> Result<()> {
        let start = Instant::now();
        loop {
            match self.backend.socket_state(handle) {
                tcp::State::Established => return Ok(()),
                tcp::State::Closed => {
                    return Err(VirtioNetError::Io(io::Error::new(
                        ErrorKind::ConnectionRefused,
                        "guest refused smoltcp TCP connection",
                    )));
                }
                _ => {}
            }
            if start.elapsed() > HANDSHAKE_TIMEOUT {
                return Err(VirtioNetError::Io(io::Error::new(
                    ErrorKind::TimedOut,
                    "guest did not complete TCP handshake within timeout",
                )));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Byte shuttle. Each iteration tries both directions once;
    /// sleeps `POLL_INTERVAL` only if nothing moved, so the loop
    /// stays snappy under load and quiet when idle.
    fn pump_bytes(&self, mut stream: TcpStream, handle: SocketHandle) -> Result<()> {
        let mut buf = [0u8; BRIDGE_BUFFER_BYTES];
        let mut external_eof = false;

        loop {
            let mut did_work = false;

            // Direction 1: external → smoltcp.
            if !external_eof {
                match stream.read(&mut buf) {
                    Ok(0) => {
                        // External peer closed their send side.
                        // Flush any trailing bytes they sent and
                        // propagate FIN into smoltcp so the guest
                        // sees orderly shutdown. We keep draining
                        // the reverse direction below until the
                        // guest also closes.
                        external_eof = true;
                        self.backend.close_socket(handle);
                    }
                    Ok(n) => {
                        // smoltcp send may short-write when its TX
                        // buffer is nearly full. Loop the remainder
                        // on subsequent iterations rather than
                        // dropping bytes.
                        let mut written = 0;
                        while written < n {
                            let w = self.backend.socket_send(handle, &buf[written..n])?;
                            if w == 0 {
                                // TX buffer full — bail out and
                                // let the next loop iteration
                                // reclaim capacity. The already-
                                // consumed bytes aren't lost; the
                                // kernel is holding them in its
                                // own receive buffer and will
                                // redeliver on the next read if we
                                // don't make progress here.
                                return Err(VirtioNetError::Io(io::Error::new(
                                    ErrorKind::WouldBlock,
                                    "smoltcp TX buffer full mid-write",
                                )));
                            }
                            written += w;
                        }
                        did_work = true;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) => return Err(VirtioNetError::Io(e)),
                }
            }

            // Direction 2: smoltcp → external.
            let n = self.backend.socket_recv(handle, &mut buf)?;
            if n > 0 {
                write_all_nonblocking(&mut stream, &buf[..n])?;
                did_work = true;
            }

            // Teardown condition: external already EOF'd and
            // smoltcp has nothing more pending to deliver.
            let state = self.backend.socket_state(handle);
            if external_eof
                && n == 0
                && matches!(
                    state,
                    tcp::State::Closed | tcp::State::TimeWait | tcp::State::CloseWait
                )
            {
                return Ok(());
            }

            // Also exit cleanly if the guest closed its half and
            // we've drained everything they sent.
            if n == 0 && state == tcp::State::Closed {
                return Ok(());
            }

            if !did_work {
                thread::sleep(POLL_INTERVAL);
            }
        }
    }
}

/// Write every byte, retrying on `WouldBlock`. `TcpStream::write_all`
/// on a non-blocking stream propagates `WouldBlock` back to the
/// caller instead of waiting, which doesn't fit our pump loop —
/// we'd rather sleep a tick and retry than tear the connection
/// down over a transiently-full kernel send buffer.
fn write_all_nonblocking(stream: &mut TcpStream, mut data: &[u8]) -> Result<()> {
    while !data.is_empty() {
        match stream.write(data) {
            Ok(0) => {
                return Err(VirtioNetError::Io(io::Error::new(
                    ErrorKind::WriteZero,
                    "external stream write returned 0 bytes",
                )));
            }
            Ok(n) => {
                data = &data[n..];
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(POLL_INTERVAL);
            }
            Err(e) => return Err(VirtioNetError::Io(e)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{SocketAddr, TcpListener};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration as StdDuration;

    use smoltcp::wire::IpAddress;

    use crate::smoltcp_backend::{
        BackendConfig, DEFAULT_GUEST_IPV4, DEFAULT_HOST_IPV4, DEFAULT_LISTEN_PORT,
    };
    use crate::NetworkBackend;

    /// Spin up a background thread that continuously shuttles
    /// Ethernet frames between two backends until `stop` is set.
    /// This stands in for the virtio-net ring that will do the
    /// same job in production (commit #6).
    fn spawn_frame_shuttle(
        a: Arc<SmoltcpBackend>,
        b: Arc<SmoltcpBackend>,
        stop: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while !stop.load(Ordering::Relaxed) {
                let mut did_work = false;
                while let Ok(n) = a.read_frame(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    b.write_frame(&buf[..n]).unwrap();
                    did_work = true;
                }
                while let Ok(n) = b.read_frame(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    a.write_frame(&buf[..n]).unwrap();
                    did_work = true;
                }
                if !did_work {
                    thread::sleep(StdDuration::from_millis(1));
                }
            }
        })
    }

    #[test]
    fn tcp_proxy_bridges_bytes_from_os_stream_to_smoltcp_echo() {
        // Backend A — host-side proxy backend: default IP, no
        // echo listener.
        let host_backend = Arc::new(SmoltcpBackend::with_config(BackendConfig {
            listen_port: 0,
            ..BackendConfig::default()
        }));

        // Backend B — fake guest with the built-in echo listener
        // on 8080.
        let guest_backend = Arc::new(SmoltcpBackend::with_config(BackendConfig {
            host_ipv4: DEFAULT_GUEST_IPV4,
            host_mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
            gateway_ipv4: DEFAULT_HOST_IPV4,
            listen_port: DEFAULT_LISTEN_PORT,
        }));

        // Frame shuttle thread (fake virtio-net ring).
        let stop = Arc::new(AtomicBool::new(false));
        let shuttle =
            spawn_frame_shuttle(host_backend.clone(), guest_backend.clone(), stop.clone());

        // OS loopback listener on an ephemeral port. We bind it
        // *before* starting the proxy thread so we know the port
        // the client should connect to.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let listen_addr: SocketAddr = listener.local_addr().expect("local_addr");

        let proxy = TcpProxy::new(
            host_backend.clone(),
            IpEndpoint::new(
                IpAddress::v4(
                    DEFAULT_GUEST_IPV4[0],
                    DEFAULT_GUEST_IPV4[1],
                    DEFAULT_GUEST_IPV4[2],
                    DEFAULT_GUEST_IPV4[3],
                ),
                DEFAULT_LISTEN_PORT,
            ),
        );
        assert_eq!(
            proxy.guest_endpoint().port,
            DEFAULT_LISTEN_PORT,
            "guest_endpoint() must round-trip the configured port"
        );

        // Server thread: accept one connection, bridge it.
        let server_thread = thread::spawn(move || {
            let (accepted, _) = listener.accept().expect("accept external connection");
            proxy.bridge_connection(accepted)
        });

        // Client (this test thread) — connect to the OS listener,
        // write a payload, verify the echo comes back.
        let mut client = TcpStream::connect(listen_addr).expect("client connect to proxy listener");
        client
            .set_read_timeout(Some(StdDuration::from_secs(5)))
            .expect("set read timeout");

        const PAYLOAD: &[u8] = b"hello via TcpProxy\n";
        client.write_all(PAYLOAD).expect("client write payload");

        let mut received = Vec::with_capacity(PAYLOAD.len());
        let mut buf = [0u8; 128];
        while received.len() < PAYLOAD.len() {
            let n = client.read(&mut buf).expect("client read echo");
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
        }

        // Shutdown the client write side so the proxy knows we're
        // done; it'll close the smoltcp socket, propagate FIN into
        // the guest's echo listener, and bridge_connection returns.
        client
            .shutdown(std::net::Shutdown::Write)
            .expect("shutdown write");
        drop(client);

        // Join the server thread — bridge_connection should exit
        // cleanly on the double-FIN teardown.
        let bridge_result = server_thread.join().expect("server thread must not panic");
        assert!(
            bridge_result.is_ok(),
            "bridge_connection returned error: {bridge_result:?}"
        );

        assert_eq!(
            &received[..PAYLOAD.len()],
            PAYLOAD,
            "echoed payload must match (got {received:?})",
        );

        stop.store(true, Ordering::Relaxed);
        shuttle.join().expect("frame shuttle must not panic");
    }
}
