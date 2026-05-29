//! Assembled virtio-vsock device: MMIO transport + virtqueues + connection table.
//!
//! This is the piece that turns the three lower layers into a working device:
//!
//! - [`MmioTransport`](crate::MmioTransport) — the register model the guest
//!   driver pokes to discover the device and program the queues.
//! - [`SplitQueue`](crate::SplitQueue) — descriptor/avail/used ring traversal
//!   over guest memory.
//! - [`ConnectionTable`](crate::ConnectionTable) — the vsock connection state
//!   machine that decides what to do with each incoming packet.
//!
//! `vm-kvm` routes the guest's MMIO exits into [`VsockDevice::mmio_read`] /
//! [`VsockDevice::mmio_write`], and after a queue kick (`QueueNotify`) calls
//! [`VsockDevice::process`] with a [`GuestRam`] view of guest memory. Process
//! drains the **tx** queue (guest→host packets) through the connection table,
//! then flushes any resulting control replies, delivered-payload
//! acknowledgements, and host-originated data onto the **rx** queue
//! (host→guest), asserting the device interrupt when it completes buffers.
//!
//! The whole device is pure state in our address space — no host-kernel
//! device object — which is what keeps a running VM snapshot- and fork-able.
//!
//! # Queue indices
//!
//! Per the virtio-vsock spec the guest exposes three queues; from the
//! *device's* point of view:
//!
//! - queue 0 (**rx**): the guest posts empty writable buffers; we fill them
//!   with host→guest packets.
//! - queue 1 (**tx**): the guest posts readable buffers holding guest→host
//!   packets; we consume them.
//! - queue 2 (**event**): config-change events; unused today.

use std::collections::VecDeque;

use crate::{
    ConnectionId, ConnectionTable, GuestRam, MmioTransport, QueueError, QueueNotify, Reply,
    TableConfig, VsockHeader, VSOCK_HDR_LEN,
};

/// Device's rx queue (host→guest): guest supplies writable buffers.
const RX_QUEUE: usize = 0;
/// Device's tx queue (guest→host): guest supplies readable buffers.
const TX_QUEUE: usize = 1;

/// A packet waiting for an rx descriptor so it can be delivered to the guest.
#[derive(Debug, Clone)]
struct OutPacket {
    header: VsockHeader,
    payload: Vec<u8>,
}

/// A fully assembled, host-side virtio-vsock device.
#[derive(Debug)]
pub struct VsockDevice {
    transport: MmioTransport,
    table: ConnectionTable,
    /// rx/tx split queues, lazily built once the driver marks them ready.
    /// Indices match [`RX_QUEUE`] / [`TX_QUEUE`].
    rx: Option<crate::SplitQueue>,
    tx: Option<crate::SplitQueue>,
    /// Host→guest packets awaiting an rx buffer.
    outbound: VecDeque<OutPacket>,
    /// Guest→host stream payloads, ready for the application layer.
    inbound: VecDeque<(ConnectionId, Vec<u8>)>,
    guest_cid: u64,
}

impl VsockDevice {
    /// Construct a vsock device advertising `guest_cid`, using `cfg` for the
    /// host-side connection table (local cid + advertised buffer size).
    pub fn new(guest_cid: u64, cfg: TableConfig) -> Self {
        Self {
            transport: MmioTransport::new_vsock(guest_cid),
            table: ConnectionTable::new(cfg),
            rx: None,
            tx: None,
            outbound: VecDeque::new(),
            inbound: VecDeque::new(),
            guest_cid,
        }
    }

    /// The guest context id this device advertises.
    pub fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    /// Borrow the underlying transport (status, features, queue config).
    pub fn transport(&self) -> &MmioTransport {
        &self.transport
    }

    /// Handle an MMIO read within the device register window.
    pub fn mmio_read(&self, offset: u64, size: usize) -> u64 {
        self.transport.read(offset, size)
    }

    /// Handle an MMIO write within the device register window. Returns the
    /// queue index if the write was a `QueueNotify` kick, so the caller knows
    /// to run [`process`](Self::process).
    pub fn mmio_write(&mut self, offset: u64, size: usize, value: u64) -> Option<u32> {
        self.transport.write(offset, size, value);
        self.transport.take_notify().map(|QueueNotify(q)| q)
    }

    /// `true` while the device's interrupt line is asserted (the guest hasn't
    /// yet acked the last used-buffer notification).
    pub fn interrupt_asserted(&self) -> bool {
        self.transport.interrupt_asserted()
    }

    /// Register a passive listener on `port`. Incoming `Request`s to a port
    /// with no listener are rejected with `Rst`.
    pub fn listen(&mut self, port: u32) -> bool {
        self.table.listen(port)
    }

    /// Stop accepting new connections on `port` (existing ones keep running).
    pub fn unlisten(&mut self, port: u32) -> bool {
        self.table.unlisten(port)
    }

    /// Queue `data` to be sent to the guest on an established connection.
    /// Returns `false` if the connection is unknown or not established. The
    /// bytes are framed into an `Rw` packet and delivered on the next
    /// [`process`](Self::process).
    pub fn send(&mut self, id: ConnectionId, data: &[u8]) -> bool {
        // Payloads larger than u32::MAX can't fit the wire `len` field; the
        // caller (proto framer) keeps frames far below that.
        let Ok(len) = u32::try_from(data.len()) else {
            return false;
        };
        match self.table.prepare_data(&id, len) {
            Some(header) => {
                self.outbound.push_back(OutPacket {
                    header,
                    payload: data.to_vec(),
                });
                true
            }
            None => false,
        }
    }

    /// Pop the next guest→host stream payload, if any.
    pub fn recv(&mut self) -> Option<(ConnectionId, Vec<u8>)> {
        self.inbound.pop_front()
    }

    /// Number of host→guest packets still waiting for an rx buffer.
    pub fn pending_outbound(&self) -> usize {
        self.outbound.len()
    }

    /// Run one device cycle: drain the tx queue through the connection table,
    /// then flush outbound packets onto the rx queue. Returns `true` if any
    /// buffer was completed (an interrupt was asserted). The caller injects
    /// the device IRQ into the guest when this returns `true`.
    pub fn process(&mut self, mem: &impl GuestRam) -> Result<bool, QueueError> {
        self.sync_queues();
        let mut completed = self.drain_tx(mem)?;
        completed |= self.flush_rx(mem)?;
        if completed {
            self.transport.raise_interrupt();
        }
        Ok(completed)
    }

    /// Bring the cached split queues in line with the transport's programmed
    /// state: build them once the driver marks them ready, and tear
    /// everything down on a device reset (status written back to 0).
    fn sync_queues(&mut self) {
        if self.transport.status() == 0 {
            self.rx = None;
            self.tx = None;
            self.outbound.clear();
            self.inbound.clear();
            return;
        }
        if self.rx.is_none() {
            if let Some(cfg) = self.transport.queue(RX_QUEUE) {
                self.rx = crate::SplitQueue::new(cfg);
            }
        }
        if self.tx.is_none() {
            if let Some(cfg) = self.transport.queue(TX_QUEUE) {
                self.tx = crate::SplitQueue::new(cfg);
            }
        }
    }

    fn drain_tx(&mut self, mem: &impl GuestRam) -> Result<bool, QueueError> {
        // Take the queue out so we can borrow `self` mutably (table, outbound)
        // inside the loop; restore it before returning, even on error.
        let Some(mut tx) = self.tx.take() else {
            return Ok(false);
        };
        let mut completed = false;
        let outcome = loop {
            match tx.pop_avail(mem) {
                Ok(Some(chain)) => {
                    match chain.read_readable(mem) {
                        Ok(raw) => self.handle_guest_packet(&raw),
                        Err(e) => break Err(e),
                    }
                    if let Err(e) = tx.push_used(mem, chain.head, 0) {
                        break Err(e);
                    }
                    completed = true;
                }
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        self.tx = Some(tx);
        outcome?;
        Ok(completed)
    }

    fn handle_guest_packet(&mut self, raw: &[u8]) {
        // A malformed header (short buffer, unknown op/type) is dropped; the
        // guest driver will time out or reset rather than us panicking.
        let Ok(hdr) = VsockHeader::from_bytes(raw) else {
            return;
        };
        match self.table.dispatch(&hdr) {
            Reply::Send(header) => self.outbound.push_back(OutPacket {
                header,
                payload: Vec::new(),
            }),
            Reply::DeliverPayload { conn_id, bytes } => {
                let start = VSOCK_HDR_LEN;
                let end = start.saturating_add(bytes as usize);
                let payload = raw.get(start..end).unwrap_or(&[]).to_vec();
                self.inbound.push_back((conn_id, payload));
            }
            Reply::None => {}
        }
    }

    fn flush_rx(&mut self, mem: &impl GuestRam) -> Result<bool, QueueError> {
        let Some(mut rx) = self.rx.take() else {
            return Ok(false);
        };
        let mut completed = false;
        let outcome = loop {
            if self.outbound.is_empty() {
                break Ok(());
            }
            match rx.pop_avail(mem) {
                Ok(Some(chain)) => {
                    // A buffer is available — commit to delivering this packet.
                    let pkt = self
                        .outbound
                        .pop_front()
                        .expect("outbound non-empty checked above");
                    let mut buf = Vec::with_capacity(VSOCK_HDR_LEN + pkt.payload.len());
                    buf.extend_from_slice(&pkt.header.to_bytes());
                    buf.extend_from_slice(&pkt.payload);
                    match chain.write_writable(mem, &buf) {
                        Ok(written) => {
                            if let Err(e) = rx.push_used(mem, chain.head, written as u32) {
                                break Err(e);
                            }
                            completed = true;
                        }
                        Err(e) => break Err(e),
                    }
                }
                // No rx buffers posted yet — leave outbound queued for later.
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        self.rx = Some(rx);
        outcome?;
        Ok(completed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::{
        STATUS_ACKNOWLEDGE, STATUS_DRIVER, STATUS_DRIVER_OK, STATUS_FEATURES_OK, VIRTIO_ID_VSOCK,
    };
    use crate::{Endpoint, VsockOp, VsockType};
    use std::cell::RefCell;

    // ---- Fake guest RAM (flat Vec, GPA 0-based) ------------------------

    struct FakeRam {
        mem: RefCell<Vec<u8>>,
    }
    impl FakeRam {
        fn new(size: usize) -> Self {
            Self {
                mem: RefCell::new(vec![0u8; size]),
            }
        }
        fn poke(&self, gpa: u64, bytes: &[u8]) {
            self.write_at(gpa, bytes).unwrap();
        }
        fn peek(&self, gpa: u64, len: usize) -> Vec<u8> {
            let mut v = vec![0u8; len];
            self.read_at(gpa, &mut v).unwrap();
            v
        }
    }
    impl GuestRam for FakeRam {
        fn read_at(&self, gpa: u64, buf: &mut [u8]) -> Result<(), QueueError> {
            let mem = self.mem.borrow();
            let start = gpa as usize;
            let end = start
                .checked_add(buf.len())
                .filter(|&e| e <= mem.len())
                .ok_or(QueueError::OutOfBounds {
                    gpa,
                    len: buf.len(),
                })?;
            buf.copy_from_slice(&mem[start..end]);
            Ok(())
        }
        fn write_at(&self, gpa: u64, buf: &[u8]) -> Result<(), QueueError> {
            let mut mem = self.mem.borrow_mut();
            let start = gpa as usize;
            let end = start
                .checked_add(buf.len())
                .filter(|&e| e <= mem.len())
                .ok_or(QueueError::OutOfBounds {
                    gpa,
                    len: buf.len(),
                })?;
            mem[start..end].copy_from_slice(buf);
            Ok(())
        }
    }

    // ---- Ring layout helpers -------------------------------------------
    //
    // Each queue gets desc/avail/used tables and a buffer pool. Queue size 8.
    //   rx:  desc 0x1000 avail 0x1200 used 0x1300 bufs 0x4000..
    //   tx:  desc 0x2000 avail 0x2200 used 0x2300 bufs 0x6000..
    const QSIZE: u32 = 8;
    const RX_DESC: u64 = 0x1000;
    const RX_AVAIL: u64 = 0x1200;
    const RX_USED: u64 = 0x1300;
    const RX_BUF: u64 = 0x4000;
    const TX_DESC: u64 = 0x2000;
    const TX_AVAIL: u64 = 0x2200;
    const TX_USED: u64 = 0x2300;
    const TX_BUF: u64 = 0x6000;
    const GUEST_CID: u64 = 3;
    const HOST_CID: u64 = 2;

    fn write_desc(ram: &FakeRam, desc_base: u64, idx: u64, addr: u64, len: u32, flags: u16) {
        let base = desc_base + 16 * idx;
        ram.poke(base, &addr.to_le_bytes());
        ram.poke(base + 8, &len.to_le_bytes());
        ram.poke(base + 12, &flags.to_le_bytes());
        ram.poke(base + 14, &0u16.to_le_bytes());
    }

    fn publish_avail(ram: &FakeRam, avail_base: u64, slot: u64, head: u16, idx: u16) {
        ram.poke(avail_base + 4 + 2 * slot, &head.to_le_bytes());
        ram.poke(avail_base + 2, &idx.to_le_bytes());
    }

    fn used_idx(ram: &FakeRam, used_base: u64) -> u16 {
        u16::from_le_bytes(ram.peek(used_base + 2, 2).try_into().unwrap())
    }

    /// Program the device's queues through MMIO and drive status to DRIVER_OK,
    /// exactly as a guest driver would.
    fn bring_up(dev: &mut VsockDevice) {
        // Select & program rx (queue 0).
        dev.mmio_write(mmio_off::QUEUE_SEL, 4, RX_QUEUE as u64);
        dev.mmio_write(mmio_off::QUEUE_NUM, 4, u64::from(QSIZE));
        dev.mmio_write(mmio_off::QUEUE_DESC_LOW, 4, RX_DESC);
        dev.mmio_write(mmio_off::QUEUE_DRIVER_LOW, 4, RX_AVAIL);
        dev.mmio_write(mmio_off::QUEUE_DEVICE_LOW, 4, RX_USED);
        dev.mmio_write(mmio_off::QUEUE_READY, 4, 1);
        // Select & program tx (queue 1).
        dev.mmio_write(mmio_off::QUEUE_SEL, 4, TX_QUEUE as u64);
        dev.mmio_write(mmio_off::QUEUE_NUM, 4, u64::from(QSIZE));
        dev.mmio_write(mmio_off::QUEUE_DESC_LOW, 4, TX_DESC);
        dev.mmio_write(mmio_off::QUEUE_DRIVER_LOW, 4, TX_AVAIL);
        dev.mmio_write(mmio_off::QUEUE_DEVICE_LOW, 4, TX_USED);
        dev.mmio_write(mmio_off::QUEUE_READY, 4, 1);
        // Status: ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK.
        dev.mmio_write(
            mmio_off::STATUS,
            4,
            u64::from(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK),
        );
    }

    // Register offsets we drive in tests (mirror of mmio::reg, which is private).
    mod mmio_off {
        pub const QUEUE_SEL: u64 = 0x030;
        pub const QUEUE_NUM: u64 = 0x038;
        pub const QUEUE_READY: u64 = 0x044;
        pub const STATUS: u64 = 0x070;
        pub const QUEUE_DESC_LOW: u64 = 0x080;
        pub const QUEUE_DRIVER_LOW: u64 = 0x090;
        pub const QUEUE_DEVICE_LOW: u64 = 0x0a0;
    }

    fn dev() -> VsockDevice {
        VsockDevice::new(
            GUEST_CID,
            TableConfig {
                local_cid: HOST_CID,
                default_buf_alloc: 64 * 1024,
            },
        )
    }

    /// Build a guest→host packet header (guest cid -> host cid).
    fn guest_pkt(op: VsockOp, src_port: u32, dst_port: u32, len: u32) -> VsockHeader {
        VsockHeader {
            src_cid: GUEST_CID,
            dst_cid: HOST_CID,
            src_port,
            dst_port,
            len,
            vtype: VsockType::Stream,
            op,
            flags: 0,
            buf_alloc: 64 * 1024,
            fwd_cnt: 0,
        }
    }

    /// Place a guest→host packet (header + payload) into the tx queue at the
    /// given descriptor/slot, and bump tx avail.idx.
    fn post_tx(ram: &FakeRam, idx: u16, hdr: &VsockHeader, payload: &[u8]) {
        let buf_addr = TX_BUF + u64::from(idx) * 0x200;
        ram.poke(buf_addr, &hdr.to_bytes());
        if !payload.is_empty() {
            ram.poke(buf_addr + VSOCK_HDR_LEN as u64, payload);
        }
        let total = VSOCK_HDR_LEN as u32 + payload.len() as u32;
        write_desc(ram, TX_DESC, u64::from(idx), buf_addr, total, 0);
        publish_avail(ram, TX_AVAIL, u64::from(idx % QSIZE as u16), idx, idx + 1);
    }

    /// Post an empty writable rx buffer at descriptor/slot `idx`.
    fn post_rx_buffer(ram: &FakeRam, idx: u16, cap: u32) {
        let buf_addr = RX_BUF + u64::from(idx) * 0x200;
        write_desc(
            ram,
            RX_DESC,
            u64::from(idx),
            buf_addr,
            cap,
            2, /* WRITE */
        );
        publish_avail(ram, RX_AVAIL, u64::from(idx % QSIZE as u16), idx, idx + 1);
    }

    fn read_rx_packet(ram: &FakeRam, idx: u16) -> VsockHeader {
        let buf_addr = RX_BUF + u64::from(idx) * 0x200;
        VsockHeader::from_bytes(&ram.peek(buf_addr, VSOCK_HDR_LEN)).unwrap()
    }

    #[test]
    fn device_advertises_vsock_after_construction() {
        let d = dev();
        assert_eq!(d.guest_cid(), GUEST_CID);
        assert_eq!(d.transport().read(0x008, 4) as u32, VIRTIO_ID_VSOCK);
    }

    #[test]
    fn request_to_listening_port_yields_response_on_rx_queue() {
        let ram = FakeRam::new(0x10000);
        let mut d = dev();
        bring_up(&mut d);
        d.listen(1024);

        // Guest posts an rx buffer for us to write the Response into.
        post_rx_buffer(&ram, 0, 256);
        // Guest sends a Request (guest:5000 -> host:1024).
        post_tx(&ram, 0, &guest_pkt(VsockOp::Request, 5000, 1024, 0), &[]);

        let irq = d.process(&ram).unwrap();
        assert!(irq, "completing buffers must assert the interrupt");
        assert!(d.interrupt_asserted());

        // tx buffer consumed, rx buffer completed.
        assert_eq!(used_idx(&ram, TX_USED), 1);
        assert_eq!(used_idx(&ram, RX_USED), 1);

        // The packet written to the guest is a Response, host->guest.
        let resp = read_rx_packet(&ram, 0);
        assert_eq!(resp.op, VsockOp::Response);
        assert_eq!(resp.src_cid, HOST_CID);
        assert_eq!(resp.dst_cid, GUEST_CID);
        assert_eq!(resp.src_port, 1024);
        assert_eq!(resp.dst_port, 5000);
    }

    #[test]
    fn request_to_closed_port_yields_rst() {
        let ram = FakeRam::new(0x10000);
        let mut d = dev();
        bring_up(&mut d);
        // No listen() on 1024.
        post_rx_buffer(&ram, 0, 256);
        post_tx(&ram, 0, &guest_pkt(VsockOp::Request, 5000, 1024, 0), &[]);

        d.process(&ram).unwrap();
        let resp = read_rx_packet(&ram, 0);
        assert_eq!(resp.op, VsockOp::Rst);
    }

    #[test]
    fn rw_payload_from_guest_is_delivered_to_application() {
        let ram = FakeRam::new(0x10000);
        let mut d = dev();
        bring_up(&mut d);
        d.listen(1024);

        // Open the connection first.
        post_rx_buffer(&ram, 0, 256);
        post_tx(&ram, 0, &guest_pkt(VsockOp::Request, 5000, 1024, 0), &[]);
        d.process(&ram).unwrap();
        assert!(d.recv().is_none(), "control packets aren't app data");

        // Guest sends data.
        let payload = b"run: echo hi";
        post_tx(
            &ram,
            1,
            &guest_pkt(VsockOp::Rw, 5000, 1024, payload.len() as u32),
            payload,
        );
        d.process(&ram).unwrap();

        let (id, data) = d.recv().expect("payload delivered");
        assert_eq!(data, payload);
        assert_eq!(id.local, Endpoint::new(HOST_CID, 1024));
        assert_eq!(id.remote, Endpoint::new(GUEST_CID, 5000));
        assert!(d.recv().is_none());
    }

    #[test]
    fn host_send_frames_data_onto_rx_queue() {
        let ram = FakeRam::new(0x10000);
        let mut d = dev();
        bring_up(&mut d);
        d.listen(1024);

        // Establish the connection (consumes rx buffer 0 for the Response).
        post_rx_buffer(&ram, 0, 256);
        post_tx(&ram, 0, &guest_pkt(VsockOp::Request, 5000, 1024, 0), &[]);
        d.process(&ram).unwrap();

        // Host pushes a reply payload to the guest.
        let id = ConnectionId::new(
            Endpoint::new(HOST_CID, 1024),
            Endpoint::new(GUEST_CID, 5000),
        );
        let reply = b"hi\n";
        assert!(d.send(id, reply));
        assert_eq!(d.pending_outbound(), 1);

        // Guest posts a fresh rx buffer; next process delivers the data.
        post_rx_buffer(&ram, 1, 256);
        let irq = d.process(&ram).unwrap();
        assert!(irq);
        assert_eq!(d.pending_outbound(), 0);

        let pkt = read_rx_packet(&ram, 1);
        assert_eq!(pkt.op, VsockOp::Rw);
        assert_eq!(pkt.len, reply.len() as u32);
        let body = ram.peek(RX_BUF + 0x200 + VSOCK_HDR_LEN as u64, reply.len());
        assert_eq!(body, reply);
    }

    #[test]
    fn send_on_unknown_connection_is_refused() {
        let mut d = dev();
        let id = ConnectionId::new(
            Endpoint::new(HOST_CID, 1024),
            Endpoint::new(GUEST_CID, 5000),
        );
        assert!(!d.send(id, b"nope"));
        assert_eq!(d.pending_outbound(), 0);
    }

    #[test]
    fn outbound_waits_when_no_rx_buffer_then_flushes() {
        let ram = FakeRam::new(0x10000);
        let mut d = dev();
        bring_up(&mut d);
        d.listen(1024);

        // Request arrives but the guest posted no rx buffer: the Response
        // can't be delivered yet and stays queued.
        post_tx(&ram, 0, &guest_pkt(VsockOp::Request, 5000, 1024, 0), &[]);
        d.process(&ram).unwrap();
        assert_eq!(
            d.pending_outbound(),
            1,
            "Response held until a buffer exists"
        );
        assert_eq!(used_idx(&ram, RX_USED), 0);

        // Guest posts a buffer; the held Response flushes out.
        post_rx_buffer(&ram, 0, 256);
        d.process(&ram).unwrap();
        assert_eq!(d.pending_outbound(), 0);
        assert_eq!(used_idx(&ram, RX_USED), 1);
        assert_eq!(read_rx_packet(&ram, 0).op, VsockOp::Response);
    }

    #[test]
    fn reset_tears_down_queues_and_buffers() {
        let ram = FakeRam::new(0x10000);
        let mut d = dev();
        bring_up(&mut d);
        d.listen(1024);
        post_tx(&ram, 0, &guest_pkt(VsockOp::Request, 5000, 1024, 0), &[]);
        d.process(&ram).unwrap();
        assert_eq!(d.pending_outbound(), 1);

        // Driver writes status 0 → full reset.
        d.mmio_write(0x070, 4, 0);
        let irq = d.process(&ram).unwrap();
        assert!(!irq);
        assert_eq!(d.pending_outbound(), 0);
        assert!(d.recv().is_none());
    }

    #[test]
    fn process_is_a_noop_before_driver_ok() {
        let ram = FakeRam::new(0x10000);
        let mut d = dev();
        // No bring_up: status is 0, queues never built.
        post_tx(&ram, 0, &guest_pkt(VsockOp::Request, 5000, 1024, 0), &[]);
        let irq = d.process(&ram).unwrap();
        assert!(!irq);
        assert_eq!(used_idx(&ram, TX_USED), 0);
    }
}
