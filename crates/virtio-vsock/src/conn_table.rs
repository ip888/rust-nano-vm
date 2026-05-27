//! Host-side connection table for virtio-vsock.
//!
//! Owns the `HashMap<ConnectionId, Connection>` and the set of
//! local ports we're listening on, and provides a single
//! [`ConnectionTable::dispatch`] entrypoint that the virtqueue
//! consumer calls with each incoming [`VsockHeader`]. The table
//! decides:
//!
//! - Whether to **accept** a `Request` (we have a `Listen` on the
//!   destination port) → create the connection, return [`Reply::Send`]
//!   with the matching `Response` header.
//! - Whether to **forward** an `Rw` packet to an existing
//!   `Established` connection → [`Reply::DeliverPayload`].
//! - Whether to **reject** the packet (no listener, unknown
//!   connection, illegal op for current state) → [`Reply::Send`] of
//!   an `Rst` header.
//! - Whether the table state changed without producing a reply
//!   (credit update, shutdown ack) → [`Reply::None`].
//!
//! The table is **transport-agnostic**: it doesn't read or write
//! payload bytes itself. The caller pulls bytes out of the rx
//! virtqueue, peels off [`VsockHeader::from_bytes`], calls
//! `dispatch`, and either ships the `Reply::Send` packet via the
//! tx queue or hands the payload slice to the application layer
//! (the proto framer) on `Reply::DeliverPayload`.
//!
//! All public methods are synchronous and take `&mut self`; the
//! caller serialises access (typically via a `Mutex` because the
//! rx and tx halves of a virtqueue run on different fds /
//! interrupts).

use std::collections::{HashMap, HashSet};

use crate::{
    Connection, ConnectionId, ConnectionState, Endpoint, VsockHeader, VsockOp, VsockType,
    HYPERVISOR_CID,
};

/// What [`ConnectionTable::dispatch`] tells the caller to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Send the contained header back over the tx queue. The
    /// caller fills the payload (usually empty for control packets).
    Send(VsockHeader),
    /// The packet was an `Rw` data frame for an existing
    /// `Established` connection. The caller should hand the
    /// payload (the `hdr.len` bytes following the header in the
    /// rx queue) to the application layer for this connection id.
    DeliverPayload {
        /// Which connection received the bytes.
        conn_id: ConnectionId,
        /// Number of payload bytes the caller must read next.
        bytes: u32,
    },
    /// Nothing to send back, nothing to deliver. The table's
    /// internal state may have changed (e.g. a credit update was
    /// absorbed).
    None,
}

/// Per-table configuration the spec leaves up to the
/// implementation.
#[derive(Debug, Clone, Copy)]
pub struct TableConfig {
    /// The host's well-known context id. Used as the `src_cid` on
    /// every outbound packet.
    pub local_cid: u64,
    /// Receive buffer size advertised to the peer on every accept
    /// (the `buf_alloc` field in our outgoing `Response` packet).
    pub default_buf_alloc: u32,
}

impl Default for TableConfig {
    fn default() -> Self {
        Self {
            local_cid: HYPERVISOR_CID,
            default_buf_alloc: 64 * 1024,
        }
    }
}

/// Host-side connection table.
#[derive(Debug)]
pub struct ConnectionTable {
    cfg: TableConfig,
    /// Local ports that have an active `listen`. A peer's `Request`
    /// to a port not in this set is rejected with `Rst`.
    listening: HashSet<u32>,
    /// All connections in any non-`Closed` state. `Closed` connections
    /// are evicted from the map.
    connections: HashMap<ConnectionId, Connection>,
}

impl ConnectionTable {
    /// Construct an empty table with the given config.
    pub fn new(cfg: TableConfig) -> Self {
        Self {
            cfg,
            listening: HashSet::new(),
            connections: HashMap::new(),
        }
    }

    /// `true` if the local port is currently in the listening set.
    pub fn is_listening(&self, port: u32) -> bool {
        self.listening.contains(&port)
    }

    /// Number of connections currently tracked.
    pub fn len(&self) -> usize {
        self.connections.len()
    }

    /// `true` when no connections are tracked.
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    /// Borrow the connection identified by `id`, if any.
    pub fn get(&self, id: &ConnectionId) -> Option<&Connection> {
        self.connections.get(id)
    }

    /// Register a passive listener on `local_port`. Returns `true`
    /// if the port was newly registered, `false` if it was already
    /// in the set.
    pub fn listen(&mut self, local_port: u32) -> bool {
        self.listening.insert(local_port)
    }

    /// Remove a listener. Existing accepted connections to that
    /// port keep running; only the willingness to accept *new*
    /// `Request`s is revoked.
    pub fn unlisten(&mut self, local_port: u32) -> bool {
        self.listening.remove(&local_port)
    }

    /// Drop the connection identified by `id`, if any. Used by the
    /// virtqueue layer when a higher level decides to close
    /// (e.g. the proto framer's stream errored).
    pub fn drop_connection(&mut self, id: &ConnectionId) -> bool {
        self.connections.remove(id).is_some()
    }

    /// Dispatch one incoming packet header. Returns the action
    /// the caller should take. Never panics on legal but
    /// unexpected packets — illegal-op transitions are surfaced as
    /// `Reply::Send(Rst)` rather than errors so the caller can
    /// drain the rx queue without branching on every packet.
    pub fn dispatch(&mut self, hdr: &VsockHeader) -> Reply {
        // From the peer's `src/dst` perspective, *our* local
        // endpoint is its `dst` and vice versa.
        let conn_id = ConnectionId {
            local: Endpoint::new(hdr.dst_cid, hdr.dst_port),
            remote: Endpoint::new(hdr.src_cid, hdr.src_port),
        };

        // We only support `VsockType::Stream`, but `VsockHeader::from_bytes`
        // already rejects any other raw type at parse time, so by the
        // time a header reaches `dispatch` we know `hdr.vtype` is
        // `Stream`. No explicit guard here.

        match hdr.op {
            VsockOp::Request => self.handle_request(conn_id, hdr),
            VsockOp::Rw => self.handle_data(conn_id, hdr),
            VsockOp::Rst => self.handle_rst(conn_id, hdr),
            VsockOp::Shutdown => self.handle_shutdown(conn_id, hdr),
            VsockOp::Response => self.handle_response(conn_id, hdr),
            VsockOp::CreditUpdate | VsockOp::CreditRequest => self.handle_credit(conn_id, hdr),
            VsockOp::Invalid => Reply::Send(self.rst_for(&conn_id, hdr)),
        }
    }

    fn handle_request(&mut self, conn_id: ConnectionId, hdr: &VsockHeader) -> Reply {
        if !self.listening.contains(&hdr.dst_port) {
            return Reply::Send(self.rst_for(&conn_id, hdr));
        }
        // Spec: a Request that names a (src, dst) pair already in
        // use is treated as a protocol error → Rst.
        if self.connections.contains_key(&conn_id) {
            return Reply::Send(self.rst_for(&conn_id, hdr));
        }
        let mut conn = Connection::new(conn_id, self.cfg.default_buf_alloc);
        if conn.listen().is_err() {
            return Reply::Send(self.rst_for(&conn_id, hdr));
        }
        if conn.recv_header(hdr).is_err() {
            return Reply::Send(self.rst_for(&conn_id, hdr));
        }
        // recv_header advanced Listen → Established. Emit the
        // matching Response packet so the peer can start sending Rw.
        self.connections.insert(conn_id, conn);
        Reply::Send(self.reply_header(&conn_id, VsockOp::Response))
    }

    fn handle_data(&mut self, conn_id: ConnectionId, hdr: &VsockHeader) -> Reply {
        let Some(conn) = self.connections.get_mut(&conn_id) else {
            return Reply::Send(self.rst_for(&conn_id, hdr));
        };
        if conn.state != ConnectionState::Established {
            return Reply::Send(self.rst_for(&conn_id, hdr));
        }
        if conn.recv_header(hdr).is_err() {
            // Connection's recv_header rejected — RST the peer.
            self.connections.remove(&conn_id);
            return Reply::Send(rst_static(self.cfg.local_cid, &conn_id, hdr.flags));
        }
        // Tell the caller how many payload bytes to drain from the
        // rx queue and ship to the proto framer.
        if hdr.len == 0 {
            Reply::None
        } else {
            Reply::DeliverPayload {
                conn_id,
                bytes: hdr.len,
            }
        }
    }

    fn handle_rst(&mut self, conn_id: ConnectionId, hdr: &VsockHeader) -> Reply {
        // Drive Connection's state machine to Closed (best effort),
        // then evict.
        if let Some(conn) = self.connections.get_mut(&conn_id) {
            let _ = conn.recv_header(hdr);
            self.connections.remove(&conn_id);
        }
        Reply::None
    }

    fn handle_shutdown(&mut self, conn_id: ConnectionId, hdr: &VsockHeader) -> Reply {
        let Some(conn) = self.connections.get_mut(&conn_id) else {
            return Reply::Send(self.rst_for(&conn_id, hdr));
        };
        if conn.recv_header(hdr).is_err() {
            self.connections.remove(&conn_id);
            return Reply::Send(rst_static(self.cfg.local_cid, &conn_id, hdr.flags));
        }
        // The state machine moves to CloseWait (or Closed); the
        // virtqueue consumer can choose to send a Shutdown ack
        // here. We don't auto-ack — let the application layer
        // decide.
        Reply::None
    }

    fn handle_response(&mut self, conn_id: ConnectionId, hdr: &VsockHeader) -> Reply {
        let Some(conn) = self.connections.get_mut(&conn_id) else {
            return Reply::Send(self.rst_for(&conn_id, hdr));
        };
        if conn.recv_header(hdr).is_err() {
            self.connections.remove(&conn_id);
            return Reply::Send(rst_static(self.cfg.local_cid, &conn_id, hdr.flags));
        }
        // Active-open path moves to Established; no reply needed.
        Reply::None
    }

    fn handle_credit(&mut self, conn_id: ConnectionId, hdr: &VsockHeader) -> Reply {
        let Some(conn) = self.connections.get_mut(&conn_id) else {
            return Reply::Send(self.rst_for(&conn_id, hdr));
        };
        let _ = conn.recv_header(hdr);
        Reply::None
    }

    /// Build an outbound `Rw` data header for an established connection,
    /// charging `len` bytes against its send credit (`tx_cnt`). Returns
    /// `None` when there's no such connection or it isn't `Established`
    /// (so the caller can't push data onto a half-open or unknown stream).
    ///
    /// The caller ships the returned header followed by the `len` payload
    /// bytes over the rx queue.
    pub fn prepare_data(&mut self, id: &ConnectionId, len: u32) -> Option<VsockHeader> {
        let conn = self.connections.get_mut(id)?;
        conn.record_send(len).ok()?;
        Some(VsockHeader {
            src_cid: id.local.cid,
            dst_cid: id.remote.cid,
            src_port: id.local.port,
            dst_port: id.remote.port,
            len,
            vtype: VsockType::Stream,
            op: VsockOp::Rw,
            flags: 0,
            buf_alloc: conn.local_buf_alloc,
            fwd_cnt: conn.fwd_cnt,
        })
    }

    /// Synthesize a header to ship a control packet *back* to the
    /// peer. Used for Response / Shutdown / CreditUpdate.
    fn reply_header(&self, conn_id: &ConnectionId, op: VsockOp) -> VsockHeader {
        let conn = self.connections.get(conn_id);
        let (buf_alloc, fwd_cnt) = conn
            .map(|c| (c.local_buf_alloc, c.fwd_cnt))
            .unwrap_or((self.cfg.default_buf_alloc, 0));
        VsockHeader {
            src_cid: conn_id.local.cid,
            dst_cid: conn_id.remote.cid,
            src_port: conn_id.local.port,
            dst_port: conn_id.remote.port,
            len: 0,
            vtype: VsockType::Stream,
            op,
            flags: 0,
            buf_alloc,
            fwd_cnt,
        }
    }

    fn rst_for(&self, conn_id: &ConnectionId, src_hdr: &VsockHeader) -> VsockHeader {
        rst_static(self.cfg.local_cid, conn_id, src_hdr.flags)
    }
}

/// Build an `Rst` header. Static helper so the `handle_*` paths can
/// call it after dropping their `&mut self` borrow on the
/// connections map.
fn rst_static(_local_cid: u64, conn_id: &ConnectionId, flags: u32) -> VsockHeader {
    VsockHeader {
        src_cid: conn_id.local.cid,
        dst_cid: conn_id.remote.cid,
        src_port: conn_id.local.port,
        dst_port: conn_id.remote.port,
        len: 0,
        vtype: VsockType::Stream,
        op: VsockOp::Rst,
        flags,
        buf_alloc: 0,
        fwd_cnt: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TableConfig {
        TableConfig {
            local_cid: 2,
            default_buf_alloc: 4096,
        }
    }

    /// Build a header from the peer to us. `peer_cid` defaults to 3.
    fn peer_header(op: VsockOp, dst_port: u32, src_port: u32) -> VsockHeader {
        VsockHeader {
            src_cid: 3,
            dst_cid: 2,
            src_port,
            dst_port,
            len: 0,
            vtype: VsockType::Stream,
            op,
            flags: 0,
            buf_alloc: 8192,
            fwd_cnt: 0,
        }
    }

    fn cid_from(hdr: &VsockHeader) -> ConnectionId {
        ConnectionId {
            local: Endpoint::new(hdr.dst_cid, hdr.dst_port),
            remote: Endpoint::new(hdr.src_cid, hdr.src_port),
        }
    }

    #[test]
    fn request_to_listening_port_is_accepted_and_returns_response() {
        let mut t = ConnectionTable::new(cfg());
        t.listen(1024);

        let req = peer_header(VsockOp::Request, 1024, 5000);
        let reply = t.dispatch(&req);
        let Reply::Send(resp) = reply else {
            panic!("expected Reply::Send, got {reply:?}");
        };
        assert_eq!(resp.op, VsockOp::Response);
        assert_eq!(resp.src_port, 1024);
        assert_eq!(resp.dst_port, 5000);
        assert_eq!(resp.src_cid, 2);
        assert_eq!(resp.dst_cid, 3);
        assert_eq!(resp.buf_alloc, 4096);

        // Connection now tracked as Established.
        let conn = t.get(&cid_from(&req)).expect("conn tracked");
        assert_eq!(conn.state, ConnectionState::Established);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn request_to_unlistening_port_is_rejected_with_rst() {
        let mut t = ConnectionTable::new(cfg());
        // No listen() — port 999 is closed.
        let req = peer_header(VsockOp::Request, 999, 4000);
        match t.dispatch(&req) {
            Reply::Send(h) => {
                assert_eq!(h.op, VsockOp::Rst);
                assert_eq!(h.src_port, 999);
                assert_eq!(h.dst_port, 4000);
            }
            other => panic!("expected RST, got {other:?}"),
        }
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn duplicate_request_for_same_pair_is_rst() {
        let mut t = ConnectionTable::new(cfg());
        t.listen(1024);
        let req = peer_header(VsockOp::Request, 1024, 5000);
        let _ = t.dispatch(&req); // first opens
        let reply = t.dispatch(&req);
        match reply {
            Reply::Send(h) => assert_eq!(h.op, VsockOp::Rst),
            other => panic!("expected RST, got {other:?}"),
        }
        // Original connection still exists.
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn rw_data_on_established_returns_deliver_payload() {
        let mut t = ConnectionTable::new(cfg());
        t.listen(1024);
        let req = peer_header(VsockOp::Request, 1024, 5000);
        let _ = t.dispatch(&req);

        let mut data = peer_header(VsockOp::Rw, 1024, 5000);
        data.len = 42;
        match t.dispatch(&data) {
            Reply::DeliverPayload { conn_id, bytes } => {
                assert_eq!(bytes, 42);
                assert_eq!(conn_id, cid_from(&data));
            }
            other => panic!("expected DeliverPayload, got {other:?}"),
        }
    }

    #[test]
    fn rw_with_zero_len_returns_none() {
        let mut t = ConnectionTable::new(cfg());
        t.listen(1024);
        let _ = t.dispatch(&peer_header(VsockOp::Request, 1024, 5000));

        let zero = peer_header(VsockOp::Rw, 1024, 5000);
        // len defaults to 0 in peer_header().
        assert_eq!(zero.len, 0);
        assert_eq!(t.dispatch(&zero), Reply::None);
    }

    #[test]
    fn rw_on_unknown_connection_is_rst() {
        let mut t = ConnectionTable::new(cfg());
        let data = peer_header(VsockOp::Rw, 1024, 5000);
        match t.dispatch(&data) {
            Reply::Send(h) => assert_eq!(h.op, VsockOp::Rst),
            other => panic!("expected RST, got {other:?}"),
        }
    }

    #[test]
    fn shutdown_moves_to_close_wait_and_no_reply() {
        let mut t = ConnectionTable::new(cfg());
        t.listen(1024);
        let _ = t.dispatch(&peer_header(VsockOp::Request, 1024, 5000));

        let mut shut = peer_header(VsockOp::Shutdown, 1024, 5000);
        shut.flags = 3; // shut_rd | shut_wr
        assert_eq!(t.dispatch(&shut), Reply::None);

        let cid = cid_from(&shut);
        let state = t.get(&cid).unwrap().state;
        assert_eq!(state, ConnectionState::CloseWait);
    }

    #[test]
    fn peer_rst_evicts_connection() {
        let mut t = ConnectionTable::new(cfg());
        t.listen(1024);
        let _ = t.dispatch(&peer_header(VsockOp::Request, 1024, 5000));
        assert_eq!(t.len(), 1);

        let rst = peer_header(VsockOp::Rst, 1024, 5000);
        assert_eq!(t.dispatch(&rst), Reply::None);
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn op_invalid_is_rejected_with_rst() {
        let mut t = ConnectionTable::new(cfg());
        t.listen(1024);
        let mut bogus = peer_header(VsockOp::Request, 1024, 5000);
        bogus.op = VsockOp::Invalid;
        match t.dispatch(&bogus) {
            Reply::Send(h) => assert_eq!(h.op, VsockOp::Rst),
            other => panic!("expected RST, got {other:?}"),
        }
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn listen_and_unlisten_round_trip() {
        let mut t = ConnectionTable::new(cfg());
        assert!(!t.is_listening(7));
        assert!(t.listen(7));
        assert!(t.is_listening(7));
        assert!(!t.listen(7)); // already there
        assert!(t.unlisten(7));
        assert!(!t.is_listening(7));
        assert!(!t.unlisten(7));
    }

    #[test]
    fn drop_connection_removes_entry() {
        let mut t = ConnectionTable::new(cfg());
        t.listen(1024);
        let req = peer_header(VsockOp::Request, 1024, 5000);
        let _ = t.dispatch(&req);
        let cid = cid_from(&req);
        assert!(t.drop_connection(&cid));
        assert!(!t.drop_connection(&cid));
        assert!(t.get(&cid).is_none());
    }

    #[test]
    fn credit_update_on_known_connection_is_absorbed_silently() {
        let mut t = ConnectionTable::new(cfg());
        t.listen(1024);
        let _ = t.dispatch(&peer_header(VsockOp::Request, 1024, 5000));

        let mut cu = peer_header(VsockOp::CreditUpdate, 1024, 5000);
        cu.buf_alloc = 32_768;
        cu.fwd_cnt = 100;
        assert_eq!(t.dispatch(&cu), Reply::None);

        let cid = cid_from(&cu);
        let conn = t.get(&cid).unwrap();
        assert_eq!(conn.peer_buf_alloc, 32_768);
        assert_eq!(conn.peer_fwd_cnt, 100);
    }

    #[test]
    fn credit_update_on_unknown_connection_is_rst() {
        let mut t = ConnectionTable::new(cfg());
        let cu = peer_header(VsockOp::CreditUpdate, 1024, 5000);
        match t.dispatch(&cu) {
            Reply::Send(h) => assert_eq!(h.op, VsockOp::Rst),
            other => panic!("expected RST, got {other:?}"),
        }
    }

    #[test]
    fn distinct_remote_ports_share_listener_independently() {
        let mut t = ConnectionTable::new(cfg());
        t.listen(1024);
        let r1 = peer_header(VsockOp::Request, 1024, 5000);
        let r2 = peer_header(VsockOp::Request, 1024, 5001);
        assert!(matches!(t.dispatch(&r1), Reply::Send(h) if h.op == VsockOp::Response));
        assert!(matches!(t.dispatch(&r2), Reply::Send(h) if h.op == VsockOp::Response));
        assert_eq!(t.len(), 2);
    }
}
