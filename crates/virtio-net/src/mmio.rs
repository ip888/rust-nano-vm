//! virtio-MMIO transport register model for a virtio-net device.
//!
//! This is the device-discovery and configuration surface the guest's
//! `virtio_net` kernel driver pokes to find and set up our device over
//! a memory-mapped register window. The module is **pure state** —
//! no KVM, no guest memory, no descriptor-ring traversal — so it is
//! fully unit-testable on any host.
//!
//! Sub-PR #C will register the device's register window with
//! `crates/vm-kvm` so guest MMIO exits (reads/writes against that
//! window) route into [`MmioTransport::read`] / [`MmioTransport::write`].
//! Sub-PR #B.2 then adds queue processing on top of this transport.
//!
//! # Why reuse the virtio-vsock pattern
//!
//! The register layout (virtio spec §4.2.2, version 2) is identical
//! across every virtio-MMIO device — only four things differ per
//! device class:
//!
//! 1. **Device ID** (register 0x008): `1` for net, `19` for vsock, ...
//! 2. **Device feature bits** (register 0x010): the set of optional
//!    protocol extensions this device offers.
//! 3. **Number of virtqueues**: net uses 2 (rx + tx) at minimum, vsock
//!    uses 3 (rx + tx + event), fs uses 2+N for N request queues.
//! 4. **Config space contents** (register >= 0x100): device-class-
//!    specific layout. For net: MAC address + link status + max queue
//!    pairs.
//!
//! Everything else — feature selection via `*FeaturesSel`, status
//! bits, queue address programming, interrupt ack — is the generic
//! virtio-MMIO contract. The virtio-vsock crate in this workspace
//! implements that same contract (see `crates/virtio-vsock/src/mmio.rs`);
//! we deliberately mirror its layout and naming so a reviewer can
//! diff the two files and see exactly what differs for net. A future
//! refactor could lift the common bits into a shared `virtio-mmio`
//! crate — not in scope for sub-PR #B.
//!
//! # Register layout (virtio spec §4.2.2, version 2)
//!
//! ```text
//!   0x000 MagicValue        R    "virt" (0x74726976)
//!   0x004 Version           R    2
//!   0x008 DeviceID          R    1 (net)
//!   0x00c VendorID          R
//!   0x010 DeviceFeatures    R    selected by DeviceFeaturesSel
//!   0x014 DeviceFeaturesSel W
//!   0x020 DriverFeatures    W    selected by DriverFeaturesSel
//!   0x024 DriverFeaturesSel W
//!   0x030 QueueSel          W
//!   0x034 QueueNumMax       R
//!   0x038 QueueNum          W
//!   0x044 QueueReady        RW
//!   0x050 QueueNotify       W
//!   0x060 InterruptStatus   R
//!   0x064 InterruptACK      W
//!   0x070 Status            RW
//!   0x080 QueueDescLow      W    } guest-physical address of the
//!   0x084 QueueDescHigh     W    } descriptor table for QueueSel
//!   0x090 QueueDriverLow    W    } available ring
//!   0x094 QueueDriverHigh   W    }
//!   0x0a0 QueueDeviceLow    W    } used ring
//!   0x0a4 QueueDeviceHigh   W    }
//!   0x0fc ConfigGeneration  R
//!   0x100 ConfigSpace       RW   device-specific (net: MAC + status + mqp)
//! ```

/// `"virt"` little-endian — the MagicValue register.
pub const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;
/// Modern virtio-MMIO (supports virtio 1.0+ feature negotiation).
pub const VIRTIO_MMIO_VERSION: u32 = 2;
/// virtio device id for a network device (virtio spec §5.1).
pub const VIRTIO_ID_NET: u32 = 1;
/// Our vendor id. Arbitrary; "NANO" in ASCII — matches virtio-vsock.
pub const NANOVM_VENDOR_ID: u32 = 0x4e41_4e4f;

// Feature bits (virtio spec §5.1.3 for net, §6 for generic).

/// `VIRTIO_NET_F_MAC` — bit 5. Device advertises a MAC address in its
/// config space (first 6 bytes). When this feature is negotiated the
/// driver uses that MAC instead of generating a random one.
pub const VIRTIO_NET_F_MAC: u64 = 1 << 5;
/// `VIRTIO_NET_F_STATUS` — bit 16. Device exposes a link-status word
/// in config space at offset 6 (2 bytes LE). Bit 0 = `LINK_UP`,
/// bit 1 = `ANNOUNCE`.
pub const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
/// `VIRTIO_F_VERSION_1` — bit 32. Non-negotiable: we require modern
/// virtio from the driver so the net header is always the 12-byte
/// (not 10-byte) layout.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Link-status bit set in the config-space `status` word: link is up.
pub const VIRTIO_NET_S_LINK_UP: u16 = 1 << 0;
/// Link-status bit set in the config-space `status` word: driver
/// should announce (gratuitous ARP). Not used by this device.
pub const VIRTIO_NET_S_ANNOUNCE: u16 = 1 << 1;

/// A virtio-net device exposes two virtqueues at minimum: rx (0) and
/// tx (1). Multi-queue, control-vq, and ctrl-mq-pair variants land
/// later; for now we hard-code two.
pub const VIRTIO_NET_NUM_QUEUES: usize = 2;
/// Device's rx queue index: guest supplies writable buffers; we fill
/// them with host→guest frames.
pub const RX_QUEUE_INDEX: u32 = 0;
/// Device's tx queue index: guest supplies readable buffers holding
/// guest→host frames; we drain them.
pub const TX_QUEUE_INDEX: u32 = 1;

/// Largest queue size we advertise (`QueueNumMax`). Power of two.
/// Matches virtio-vsock in this workspace; the Linux `virtio_net`
/// driver happily picks any value `<=` this one.
pub const QUEUE_SIZE_MAX: u32 = 256;

// Device status bits (virtio spec §2.1).
/// Guest has found the device and knows how to drive it.
pub const STATUS_ACKNOWLEDGE: u32 = 1;
/// Guest has a driver for the device.
pub const STATUS_DRIVER: u32 = 2;
/// Driver is set up and ready to drive the device.
pub const STATUS_DRIVER_OK: u32 = 4;
/// Driver has finished feature negotiation.
pub const STATUS_FEATURES_OK: u32 = 8;
/// Device has experienced an unrecoverable error.
pub const STATUS_DEVICE_NEEDS_RESET: u32 = 64;
/// Something went wrong; driver has given up.
pub const STATUS_FAILED: u32 = 128;

/// `InterruptStatus` bit: the device updated a used ring and the driver
/// should process completed buffers (virtio spec §4.2.2).
pub const VIRTIO_MMIO_INT_VRING: u32 = 1;
/// `InterruptStatus` bit: config space changed; driver should re-read.
pub const VIRTIO_MMIO_INT_CONFIG: u32 = 2;

// Register offsets.
mod reg {
    pub const MAGIC: u64 = 0x000;
    pub const VERSION: u64 = 0x004;
    pub const DEVICE_ID: u64 = 0x008;
    pub const VENDOR_ID: u64 = 0x00c;
    pub const DEVICE_FEATURES: u64 = 0x010;
    pub const DEVICE_FEATURES_SEL: u64 = 0x014;
    pub const DRIVER_FEATURES: u64 = 0x020;
    pub const DRIVER_FEATURES_SEL: u64 = 0x024;
    pub const QUEUE_SEL: u64 = 0x030;
    pub const QUEUE_NUM_MAX: u64 = 0x034;
    pub const QUEUE_NUM: u64 = 0x038;
    pub const QUEUE_READY: u64 = 0x044;
    pub const QUEUE_NOTIFY: u64 = 0x050;
    pub const INTERRUPT_STATUS: u64 = 0x060;
    pub const INTERRUPT_ACK: u64 = 0x064;
    pub const STATUS: u64 = 0x070;
    pub const QUEUE_DESC_LOW: u64 = 0x080;
    pub const QUEUE_DESC_HIGH: u64 = 0x084;
    pub const QUEUE_DRIVER_LOW: u64 = 0x090;
    pub const QUEUE_DRIVER_HIGH: u64 = 0x094;
    pub const QUEUE_DEVICE_LOW: u64 = 0x0a0;
    pub const QUEUE_DEVICE_HIGH: u64 = 0x0a4;
    pub const CONFIG_GENERATION: u64 = 0x0fc;
    pub const CONFIG_SPACE: u64 = 0x100;
}

/// Per-virtqueue configuration the driver programs through the MMIO
/// registers. The transport just records these; the queue consumer
/// (sub-PR #B.2) reads them to locate the rings in guest memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueConfig {
    /// Number of descriptors the driver chose (`QueueNum`). 0 until set.
    pub size: u32,
    /// `true` once the driver has set `QueueReady = 1`.
    pub ready: bool,
    /// Guest-physical address of the descriptor table.
    pub desc: u64,
    /// Guest-physical address of the available ring (driver area).
    pub driver: u64,
    /// Guest-physical address of the used ring (device area).
    pub device: u64,
}

/// A notification raised by a `QueueNotify` write: the index of the
/// queue the guest kicked. The queue consumer acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueNotify(pub u32);

/// Device-specific config space laid out per the virtio-net spec
/// (§5.1.4, with VIRTIO_NET_F_MAC + VIRTIO_NET_F_STATUS negotiated):
///
/// ```text
///   offset 0  mac[6]      MAC address
///   offset 6  status      __le16  (LINK_UP | ANNOUNCE)
///   offset 8  max_virtqueue_pairs  __le16  (1 while we don't do MQ)
///   offset 10 mtu         __le16  (unused; VIRTIO_NET_F_MTU not advertised)
/// ```
///
/// We allocate the full 12-byte block even though we don't advertise
/// MTU, so a driver reading past the status word doesn't fall off the
/// end of the config space.
pub const CONFIG_SIZE: usize = 12;

/// virtio-MMIO transport register block for a virtio-net device.
///
/// Construct with [`MmioTransport::new_net`]. Drive from guest MMIO
/// exits via [`read`](Self::read) / [`write`](Self::write). Inspect
/// negotiated state ([`status`](Self::status), [`queue`](Self::queue),
/// [`driver_ok`](Self::driver_ok)) from the device layer above.
///
/// # Rust concept: no interior mutability needed
///
/// Methods take `&self` for reads and `&mut self` for writes — the
/// natural borrow-checker-visible shape. The device wrapper
/// ([`crate::VirtioNetDevice`]) owns this struct behind a lock, and
/// the KVM MMIO exit handler is the sole writer, so we don't need
/// `Mutex`/`RwLock` inside the transport itself. That keeps every
/// read branch-free and side-effect-free; writes are grouped under
/// one match arm.
#[derive(Debug)]
pub struct MmioTransport {
    device_id: u32,
    device_features: u64,
    /// Device-specific config space (MAC + status + max_virtqueue_pairs).
    config: Vec<u8>,

    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,
    status: u32,
    queue_sel: u32,
    queues: Vec<QueueConfig>,
    interrupt_status: u32,
    config_generation: u32,
    /// Set when the driver writes `QueueNotify`; the queue consumer
    /// drains it with [`take_notify`](Self::take_notify).
    pending_notify: Option<QueueNotify>,
}

impl MmioTransport {
    /// Construct a virtio-net transport that advertises `mac` and
    /// starts with the link up. Features = `VIRTIO_F_VERSION_1 |
    /// VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS` — the minimum a modern
    /// Linux guest driver expects.
    pub fn new_net(mac: [u8; 6]) -> Self {
        let mut config = vec![0u8; CONFIG_SIZE];
        config[0..6].copy_from_slice(&mac);
        // Link is up out of the gate — the TAP backend attaches to a
        // host-side bridge in sub-PR #D; before that, the TAP exists
        // but has no bridge. We still advertise LINK_UP because
        // Linux's virtio_net driver will refuse to bring the
        // interface up otherwise.
        config[6..8].copy_from_slice(&VIRTIO_NET_S_LINK_UP.to_le_bytes());
        // max_virtqueue_pairs = 1 (we don't advertise MQ).
        config[8..10].copy_from_slice(&1u16.to_le_bytes());
        // mtu = 0 (we don't advertise VIRTIO_NET_F_MTU; driver ignores).
        config[10..12].copy_from_slice(&0u16.to_le_bytes());
        Self {
            device_id: VIRTIO_ID_NET,
            device_features: VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS,
            config,
            device_features_sel: 0,
            driver_features: 0,
            driver_features_sel: 0,
            status: 0,
            queue_sel: 0,
            queues: vec![QueueConfig::default(); VIRTIO_NET_NUM_QUEUES],
            interrupt_status: 0,
            config_generation: 0,
            pending_notify: None,
        }
    }

    /// Current device status register value.
    pub fn status(&self) -> u32 {
        self.status
    }

    /// `true` once the driver has set `DRIVER_OK` (device is live).
    pub fn driver_ok(&self) -> bool {
        self.status & STATUS_DRIVER_OK != 0
    }

    /// Features the driver accepted. The device's own feature set is
    /// AND'd with what the driver wrote back — this is the authoritative
    /// value queue processing should look at.
    pub fn negotiated_features(&self) -> u64 {
        self.device_features & self.driver_features
    }

    /// Borrow a queue's configuration by index, if in range.
    pub fn queue(&self, index: usize) -> Option<&QueueConfig> {
        self.queues.get(index)
    }

    /// Take any pending `QueueNotify` (clears it). The queue consumer
    /// calls this after a write to learn which queue the guest kicked.
    pub fn take_notify(&mut self) -> Option<QueueNotify> {
        self.pending_notify.take()
    }

    /// Assert the used-buffer interrupt by setting the `VRING` bit in
    /// `InterruptStatus`. After this the host injects the device's IRQ
    /// into the guest; the guest clears it by writing `InterruptACK`.
    pub fn raise_vring_interrupt(&mut self) {
        self.interrupt_status |= VIRTIO_MMIO_INT_VRING;
    }

    /// Assert the config-changed interrupt bit. The driver re-reads
    /// config space when it sees this (e.g. the link went up/down).
    pub fn raise_config_interrupt(&mut self) {
        self.interrupt_status |= VIRTIO_MMIO_INT_CONFIG;
    }

    /// `true` while any `InterruptStatus` bit is set (IRQ line high).
    pub fn interrupt_asserted(&self) -> bool {
        self.interrupt_status != 0
    }

    /// Update the link-status word in config space and bump the
    /// config generation. The caller is expected to also call
    /// [`raise_config_interrupt`](Self::raise_config_interrupt) so the
    /// driver re-reads.
    pub fn set_link_status(&mut self, status: u16) {
        self.config[6..8].copy_from_slice(&status.to_le_bytes());
        self.config_generation = self.config_generation.wrapping_add(1);
    }

    /// Handle an MMIO read of `size` bytes at `offset` within the
    /// device's register window. Returns the value zero-extended into
    /// a u64. Control registers are 32-bit; config space (`>= 0x100`)
    /// supports byte/halfword/word reads.
    pub fn read(&self, offset: u64, size: usize) -> u64 {
        if offset >= reg::CONFIG_SPACE {
            return self.read_config(offset - reg::CONFIG_SPACE, size);
        }
        // Control registers are always 32-bit accesses.
        let val = match offset {
            reg::MAGIC => VIRTIO_MMIO_MAGIC,
            reg::VERSION => VIRTIO_MMIO_VERSION,
            reg::DEVICE_ID => self.device_id,
            reg::VENDOR_ID => NANOVM_VENDOR_ID,
            reg::DEVICE_FEATURES => self.read_device_features(),
            reg::QUEUE_NUM_MAX => QUEUE_SIZE_MAX,
            reg::QUEUE_READY => self.current_queue().map(|q| q.ready as u32).unwrap_or(0),
            reg::INTERRUPT_STATUS => self.interrupt_status,
            reg::STATUS => self.status,
            reg::CONFIG_GENERATION => self.config_generation,
            // Write-only or unimplemented registers read as 0, per spec.
            _ => 0,
        };
        val as u64
    }

    /// Handle an MMIO write of `size` bytes (`value` holds the low
    /// `size` bytes) at `offset`.
    pub fn write(&mut self, offset: u64, size: usize, value: u64) {
        if offset >= reg::CONFIG_SPACE {
            self.write_config(offset - reg::CONFIG_SPACE, size, value);
            return;
        }
        let v = value as u32;
        match offset {
            reg::DEVICE_FEATURES_SEL => self.device_features_sel = v,
            reg::DRIVER_FEATURES => self.write_driver_features(v),
            reg::DRIVER_FEATURES_SEL => self.driver_features_sel = v,
            reg::QUEUE_SEL => self.queue_sel = v,
            reg::QUEUE_NUM => self.with_current_queue(|q| q.size = v),
            reg::QUEUE_READY => self.with_current_queue(|q| q.ready = v & 1 != 0),
            reg::QUEUE_NOTIFY => self.pending_notify = Some(QueueNotify(v)),
            reg::INTERRUPT_ACK => self.interrupt_status &= !v,
            reg::STATUS => self.write_status(v),
            reg::QUEUE_DESC_LOW => self.with_current_queue(|q| q.desc = set_low(q.desc, v)),
            reg::QUEUE_DESC_HIGH => self.with_current_queue(|q| q.desc = set_high(q.desc, v)),
            reg::QUEUE_DRIVER_LOW => self.with_current_queue(|q| q.driver = set_low(q.driver, v)),
            reg::QUEUE_DRIVER_HIGH => self.with_current_queue(|q| q.driver = set_high(q.driver, v)),
            reg::QUEUE_DEVICE_LOW => self.with_current_queue(|q| q.device = set_low(q.device, v)),
            reg::QUEUE_DEVICE_HIGH => self.with_current_queue(|q| q.device = set_high(q.device, v)),
            // Read-only or unimplemented registers ignore writes.
            _ => {}
        }
    }

    fn read_device_features(&self) -> u32 {
        // Driver reads features 32 bits at a time, selecting the
        // low/high half via DeviceFeaturesSel.
        match self.device_features_sel {
            0 => self.device_features as u32,
            1 => (self.device_features >> 32) as u32,
            _ => 0,
        }
    }

    fn write_driver_features(&mut self, v: u32) {
        match self.driver_features_sel {
            0 => {
                self.driver_features =
                    (self.driver_features & 0xffff_ffff_0000_0000) | u64::from(v);
            }
            1 => {
                self.driver_features =
                    (self.driver_features & 0x0000_0000_ffff_ffff) | (u64::from(v) << 32);
            }
            _ => {}
        }
    }

    fn write_status(&mut self, v: u32) {
        // Writing 0 resets the device (virtio spec §2.1.1).
        if v == 0 {
            self.reset();
        } else {
            self.status = v;
        }
    }

    fn reset(&mut self) {
        self.status = 0;
        self.driver_features = 0;
        self.driver_features_sel = 0;
        self.device_features_sel = 0;
        self.queue_sel = 0;
        self.interrupt_status = 0;
        self.pending_notify = None;
        for q in &mut self.queues {
            *q = QueueConfig::default();
        }
    }

    fn current_queue(&self) -> Option<&QueueConfig> {
        self.queues.get(self.queue_sel as usize)
    }

    fn with_current_queue(&mut self, f: impl FnOnce(&mut QueueConfig)) {
        if let Some(q) = self.queues.get_mut(self.queue_sel as usize) {
            f(q);
        }
    }

    fn read_config(&self, off: u64, size: usize) -> u64 {
        let off = off as usize;
        let mut out = 0u64;
        for i in 0..size {
            let byte = self.config.get(off + i).copied().unwrap_or(0);
            out |= u64::from(byte) << (8 * i);
        }
        out
    }

    fn write_config(&mut self, off: u64, size: usize, value: u64) {
        // Net config is read-only in practice (MAC + link status are
        // device-advertised), but honor writes within bounds so a
        // misbehaving driver can't panic us; bump config_generation.
        let off = off as usize;
        let mut wrote = false;
        for i in 0..size {
            if let Some(slot) = self.config.get_mut(off + i) {
                *slot = (value >> (8 * i)) as u8;
                wrote = true;
            }
        }
        if wrote {
            self.config_generation = self.config_generation.wrapping_add(1);
        }
    }
}

fn set_low(addr: u64, low: u32) -> u64 {
    (addr & 0xffff_ffff_0000_0000) | u64::from(low)
}

fn set_high(addr: u64, high: u32) -> u64 {
    (addr & 0x0000_0000_ffff_ffff) | (u64::from(high) << 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

    fn net() -> MmioTransport {
        MmioTransport::new_net(SAMPLE_MAC)
    }

    #[test]
    fn discovery_registers_report_a_modern_net_device() {
        let t = net();
        assert_eq!(t.read(reg::MAGIC, 4) as u32, VIRTIO_MMIO_MAGIC);
        assert_eq!(t.read(reg::VERSION, 4) as u32, VIRTIO_MMIO_VERSION);
        assert_eq!(t.read(reg::DEVICE_ID, 4) as u32, VIRTIO_ID_NET);
        assert_eq!(t.read(reg::VENDOR_ID, 4) as u32, NANOVM_VENDOR_ID);
        assert_eq!(t.read(reg::QUEUE_NUM_MAX, 4) as u32, QUEUE_SIZE_MAX);
    }

    #[test]
    fn device_features_are_read_in_two_halves() {
        let mut t = net();
        // Low half: VIRTIO_NET_F_MAC (bit 5) | VIRTIO_NET_F_STATUS (bit 16)
        t.write(reg::DEVICE_FEATURES_SEL, 4, 0);
        let low = t.read(reg::DEVICE_FEATURES, 4) as u32;
        assert!(low & (1 << 5) != 0, "F_MAC must be advertised");
        assert!(low & (1 << 16) != 0, "F_STATUS must be advertised");
        // High half: VIRTIO_F_VERSION_1 is bit 32 → bit 0 of the high word.
        t.write(reg::DEVICE_FEATURES_SEL, 4, 1);
        assert_eq!(t.read(reg::DEVICE_FEATURES, 4) as u32, 1);
    }

    #[test]
    fn driver_features_accept_version_1_and_mac() {
        let mut t = net();
        // Guest driver accepts: F_MAC (bit 5), F_STATUS (bit 16),
        // F_VERSION_1 (bit 32).
        t.write(reg::DRIVER_FEATURES_SEL, 4, 0);
        t.write(reg::DRIVER_FEATURES, 4, (1 << 5) | (1 << 16));
        t.write(reg::DRIVER_FEATURES_SEL, 4, 1);
        t.write(reg::DRIVER_FEATURES, 4, 1); // bit 32
        let want = VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS;
        assert_eq!(t.negotiated_features(), want);
    }

    #[test]
    fn negotiated_features_ignore_bits_the_device_did_not_offer() {
        // A buggy driver could try to set a feature bit we don't advertise.
        // negotiated_features() must mask it off, not blindly trust the driver.
        let mut t = net();
        t.write(reg::DRIVER_FEATURES_SEL, 4, 0);
        // bit 7 (VIRTIO_NET_F_MAC_ADDR) — not advertised by us
        t.write(reg::DRIVER_FEATURES, 4, 1 << 7);
        assert_eq!(t.negotiated_features() & (1 << 7), 0);
    }

    #[test]
    fn status_progression_and_driver_ok() {
        let mut t = net();
        assert!(!t.driver_ok());
        let bits = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK;
        t.write(reg::STATUS, 4, u64::from(bits));
        assert_eq!(t.status(), bits);
        assert!(t.driver_ok());
    }

    #[test]
    fn writing_status_zero_resets_everything() {
        let mut t = net();
        t.write(
            reg::STATUS,
            4,
            u64::from(STATUS_ACKNOWLEDGE | STATUS_DRIVER),
        );
        t.write(reg::DRIVER_FEATURES, 4, 0x55);
        t.write(reg::QUEUE_SEL, 4, 1);
        t.write(reg::QUEUE_NUM, 4, 128);
        t.write(reg::STATUS, 4, 0);
        assert_eq!(t.status(), 0);
        assert_eq!(t.negotiated_features(), 0);
        assert_eq!(t.queue(1).unwrap().size, 0);
    }

    #[test]
    fn two_queues_are_programmed_independently() {
        let mut t = net();
        // Program TX (queue 1).
        t.write(reg::QUEUE_SEL, 4, TX_QUEUE_INDEX.into());
        t.write(reg::QUEUE_NUM, 4, 64);
        t.write(reg::QUEUE_DESC_LOW, 4, 0x1000);
        t.write(reg::QUEUE_DESC_HIGH, 4, 0xab);
        t.write(reg::QUEUE_DRIVER_LOW, 4, 0x2000);
        t.write(reg::QUEUE_DEVICE_LOW, 4, 0x3000);
        t.write(reg::QUEUE_READY, 4, 1);
        let q = t.queue(TX_QUEUE_INDEX as usize).unwrap();
        assert_eq!(q.size, 64);
        assert_eq!(q.desc, 0x0000_00ab_0000_1000);
        assert_eq!(q.driver, 0x2000);
        assert_eq!(q.device, 0x3000);
        assert!(q.ready);
        // RX is untouched.
        let rx = t.queue(RX_QUEUE_INDEX as usize).unwrap();
        assert_eq!(rx.size, 0);
        assert!(!rx.ready);
    }

    #[test]
    fn queue_notify_captures_rx_and_tx_kicks() {
        let mut t = net();
        t.write(reg::QUEUE_NOTIFY, 4, u64::from(RX_QUEUE_INDEX));
        assert_eq!(t.take_notify(), Some(QueueNotify(RX_QUEUE_INDEX)));
        assert_eq!(t.take_notify(), None);
        t.write(reg::QUEUE_NOTIFY, 4, u64::from(TX_QUEUE_INDEX));
        assert_eq!(t.take_notify(), Some(QueueNotify(TX_QUEUE_INDEX)));
    }

    #[test]
    fn vring_and_config_interrupt_bits_cohabit() {
        let mut t = net();
        t.raise_vring_interrupt();
        t.raise_config_interrupt();
        assert_eq!(
            t.read(reg::INTERRUPT_STATUS, 4),
            u64::from(VIRTIO_MMIO_INT_VRING | VIRTIO_MMIO_INT_CONFIG),
        );
        // ACK only the vring bit; config bit stays set.
        t.write(reg::INTERRUPT_ACK, 4, u64::from(VIRTIO_MMIO_INT_VRING));
        assert_eq!(
            t.read(reg::INTERRUPT_STATUS, 4),
            u64::from(VIRTIO_MMIO_INT_CONFIG),
        );
    }

    #[test]
    fn config_space_exposes_mac_at_offset_zero() {
        let t = net();
        // 6-byte MAC read, little-endian decode. Bytes in config are
        // raw MAC order (byte 0 is the first octet of the address,
        // which read_config returns at bit position 0).
        for (i, byte) in SAMPLE_MAC.iter().enumerate() {
            let v = t.read(reg::CONFIG_SPACE + i as u64, 1) as u8;
            assert_eq!(v, *byte, "mac byte {i}");
        }
    }

    #[test]
    fn config_space_exposes_link_up_status() {
        let t = net();
        let status = t.read(reg::CONFIG_SPACE + 6, 2) as u16;
        assert_eq!(status & VIRTIO_NET_S_LINK_UP, VIRTIO_NET_S_LINK_UP);
    }

    #[test]
    fn config_space_max_virtqueue_pairs_is_one() {
        let t = net();
        let mqp = t.read(reg::CONFIG_SPACE + 8, 2) as u16;
        assert_eq!(mqp, 1);
    }

    #[test]
    fn set_link_status_bumps_config_generation() {
        let mut t = net();
        let gen0 = t.read(reg::CONFIG_GENERATION, 4) as u32;
        t.set_link_status(0); // link went down
        let gen1 = t.read(reg::CONFIG_GENERATION, 4) as u32;
        assert_eq!(gen1, gen0.wrapping_add(1));
        let status = t.read(reg::CONFIG_SPACE + 6, 2) as u16;
        assert_eq!(status, 0);
    }

    #[test]
    fn unknown_register_reads_zero_and_write_only_regs_read_zero() {
        let t = net();
        assert_eq!(t.read(0x0d0, 4), 0); // gap / unimplemented
        assert_eq!(t.read(reg::QUEUE_NOTIFY, 4), 0); // write-only
        assert_eq!(t.read(reg::DRIVER_FEATURES, 4), 0); // write-only
    }

    #[test]
    fn out_of_range_queue_selector_is_ignored_not_panicking() {
        let mut t = net();
        t.write(reg::QUEUE_SEL, 4, 99); // no such queue
        t.write(reg::QUEUE_NUM, 4, 16); // must not panic
        assert_eq!(t.read(reg::QUEUE_READY, 4), 0);
        assert!(t.queue(99).is_none());
    }

    #[test]
    fn config_writes_within_bounds_bump_generation_but_do_not_panic_oob() {
        let mut t = net();
        // Write far past the config end: no-op, no panic, generation unchanged.
        let gen0 = t.read(reg::CONFIG_GENERATION, 4) as u32;
        t.write(reg::CONFIG_SPACE + CONFIG_SIZE as u64 + 100, 4, 0xdeadbeef);
        let gen1 = t.read(reg::CONFIG_GENERATION, 4) as u32;
        assert_eq!(gen0, gen1);
    }
}
