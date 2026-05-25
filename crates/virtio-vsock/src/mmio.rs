//! virtio-MMIO transport register model (virtio 1.x, modern/version-2).
//!
//! This is the device-discovery and configuration surface a guest
//! driver pokes to find and set up a virtio device over a
//! memory-mapped register window. It is **pure state** — no KVM, no
//! guest memory, no virtqueue traversal — so it's fully
//! unit-testable on any host. `vm-kvm` will route the guest's MMIO
//! exits (reads/writes against the device's register window) into
//! [`MmioTransport::read`] / [`MmioTransport::write`]; the virtqueue
//! plumbing that actually moves packets lands in a later slice.
//!
//! Register layout per the virtio spec §4.2.2 (MMIO Device Register
//! Layout). We implement the modern (version 2) subset a Linux
//! `virtio_mmio` driver touches to bring up a vsock device:
//!
//! ```text
//!   0x000 MagicValue        R    "virt" (0x74726976)
//!   0x004 Version           R    2
//!   0x008 DeviceID          R    19 (vsock)
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
//!   0x090 QueueDriverLow    W    } avail ring
//!   0x094 QueueDriverHigh   W    }
//!   0x0a0 QueueDeviceLow    W    } used ring
//!   0x0a4 QueueDeviceHigh   W    }
//!   0x0fc ConfigGeneration  R
//!   0x100 ConfigSpace       RW   device-specific (vsock: guest_cid)
//! ```
//!
//! All register accesses are little-endian 32-bit, except the
//! device-specific config space (`>= 0x100`), which the driver may
//! read at byte/word granularity.

/// `"virt"` little-endian — the MagicValue register.
pub const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;
/// Modern virtio-MMIO (supports virtio 1.0+ feature negotiation).
pub const VIRTIO_MMIO_VERSION: u32 = 2;
/// virtio device id for a vsock device (virtio spec §5.10).
pub const VIRTIO_ID_VSOCK: u32 = 19;
/// Our vendor id. Arbitrary; "NANO" in ASCII.
pub const NANOVM_VENDOR_ID: u32 = 0x4e41_4e4f;
/// `VIRTIO_F_VERSION_1` — bit 32. We require modern virtio.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;
/// A vsock device exposes three virtqueues: rx, tx, event.
pub const VIRTIO_VSOCK_NUM_QUEUES: usize = 3;
/// Largest queue size we advertise (QueueNumMax). Power of two.
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

/// Per-virtqueue configuration the driver programs through the
/// MMIO registers. The transport just records these; the virtqueue
/// consumer (later slice) reads them to locate the rings in guest
/// memory.
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
/// queue the guest kicked. The virtqueue consumer acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueNotify(pub u32);

/// virtio-MMIO transport register block for a single device.
///
/// Construct with [`MmioTransport::new_vsock`] for a vsock device.
/// Drive it from MMIO exits via [`read`](Self::read) /
/// [`write`](Self::write). Inspect negotiated state
/// ([`status`](Self::status), [`queue`](Self::queue),
/// [`driver_ok`](Self::driver_ok)) from the device implementation.
#[derive(Debug)]
pub struct MmioTransport {
    device_id: u32,
    device_features: u64,
    /// Device-specific config space (e.g. vsock guest_cid, 8 bytes LE).
    config: Vec<u8>,

    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,
    status: u32,
    queue_sel: u32,
    queues: Vec<QueueConfig>,
    interrupt_status: u32,
    config_generation: u32,
    /// Set when the driver writes `QueueNotify`; the consumer drains it.
    pending_notify: Option<QueueNotify>,
}

impl MmioTransport {
    /// Construct a vsock transport advertising `guest_cid` in its
    /// device config space. Features = `VIRTIO_F_VERSION_1` only
    /// (modern virtio; no optional vsock features yet).
    pub fn new_vsock(guest_cid: u64) -> Self {
        Self {
            device_id: VIRTIO_ID_VSOCK,
            device_features: VIRTIO_F_VERSION_1,
            config: guest_cid.to_le_bytes().to_vec(),
            device_features_sel: 0,
            driver_features: 0,
            driver_features_sel: 0,
            status: 0,
            queue_sel: 0,
            queues: vec![QueueConfig::default(); VIRTIO_VSOCK_NUM_QUEUES],
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

    /// Features the driver accepted (the intersection it wrote back).
    pub fn negotiated_features(&self) -> u64 {
        self.driver_features
    }

    /// Borrow a queue's configuration by index, if in range.
    pub fn queue(&self, index: usize) -> Option<&QueueConfig> {
        self.queues.get(index)
    }

    /// Take any pending `QueueNotify` (clears it). The virtqueue
    /// consumer calls this after a write to learn which queue the
    /// guest kicked.
    pub fn take_notify(&mut self) -> Option<QueueNotify> {
        self.pending_notify.take()
    }

    /// Handle an MMIO read of `size` bytes at `offset` within the
    /// device's register window. Returns the value (zero-extended
    /// into a u64). Control registers are 32-bit; config space
    /// (`>= 0x100`) supports byte/halfword/word reads.
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

    /// Handle an MMIO write of `size` bytes (`value` holds the
    /// low `size` bytes) at `offset`.
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
                self.driver_features = (self.driver_features & 0xffff_ffff_0000_0000) | u64::from(v)
            }
            1 => {
                self.driver_features =
                    (self.driver_features & 0x0000_0000_ffff_ffff) | (u64::from(v) << 32)
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
        // The vsock config (guest_cid) is read-only from the driver's
        // side in practice, but honor writes within bounds so a
        // misbehaving driver can't panic us; bumps config_generation.
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

    fn vsock() -> MmioTransport {
        MmioTransport::new_vsock(3)
    }

    #[test]
    fn discovery_registers_report_a_modern_vsock_device() {
        let t = vsock();
        assert_eq!(t.read(reg::MAGIC, 4) as u32, VIRTIO_MMIO_MAGIC);
        assert_eq!(t.read(reg::VERSION, 4) as u32, VIRTIO_MMIO_VERSION);
        assert_eq!(t.read(reg::DEVICE_ID, 4) as u32, VIRTIO_ID_VSOCK);
        assert_eq!(t.read(reg::VENDOR_ID, 4) as u32, NANOVM_VENDOR_ID);
        assert_eq!(t.read(reg::QUEUE_NUM_MAX, 4) as u32, QUEUE_SIZE_MAX);
    }

    #[test]
    fn device_features_are_read_in_two_halves() {
        let mut t = vsock();
        // Low half: no bits below 32 are set.
        t.write(reg::DEVICE_FEATURES_SEL, 4, 0);
        assert_eq!(t.read(reg::DEVICE_FEATURES, 4) as u32, 0);
        // High half: VIRTIO_F_VERSION_1 is bit 32 → bit 0 of the high word.
        t.write(reg::DEVICE_FEATURES_SEL, 4, 1);
        assert_eq!(t.read(reg::DEVICE_FEATURES, 4) as u32, 1);
    }

    #[test]
    fn driver_features_accumulate_across_both_halves() {
        let mut t = vsock();
        t.write(reg::DRIVER_FEATURES_SEL, 4, 0);
        t.write(reg::DRIVER_FEATURES, 4, 0xdead_beef);
        t.write(reg::DRIVER_FEATURES_SEL, 4, 1);
        t.write(reg::DRIVER_FEATURES, 4, 1); // accept VERSION_1
        assert_eq!(t.negotiated_features(), (1u64 << 32) | 0xdead_beef);
    }

    #[test]
    fn status_progression_and_driver_ok() {
        let mut t = vsock();
        assert!(!t.driver_ok());
        let bits = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK;
        t.write(reg::STATUS, 4, u64::from(bits));
        assert_eq!(t.status(), bits);
        assert!(t.driver_ok());
    }

    #[test]
    fn writing_status_zero_resets_everything() {
        let mut t = vsock();
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
    fn queue_config_is_programmed_per_selected_queue() {
        let mut t = vsock();
        // Program queue 1 (tx).
        t.write(reg::QUEUE_SEL, 4, 1);
        t.write(reg::QUEUE_NUM, 4, 64);
        t.write(reg::QUEUE_DESC_LOW, 4, 0x1000);
        t.write(reg::QUEUE_DESC_HIGH, 4, 0xab);
        t.write(reg::QUEUE_DRIVER_LOW, 4, 0x2000);
        t.write(reg::QUEUE_DEVICE_LOW, 4, 0x3000);
        t.write(reg::QUEUE_READY, 4, 1);

        let q = t.queue(1).unwrap();
        assert_eq!(q.size, 64);
        assert_eq!(q.desc, 0x0000_00ab_0000_1000);
        assert_eq!(q.driver, 0x2000);
        assert_eq!(q.device, 0x3000);
        assert!(q.ready);
        // QueueReady reads back the selected queue's readiness.
        assert_eq!(t.read(reg::QUEUE_READY, 4), 1);

        // Queue 0 is untouched.
        assert_eq!(t.queue(0).unwrap().size, 0);
        assert!(!t.queue(0).unwrap().ready);
    }

    #[test]
    fn queue_notify_is_captured_then_drained_once() {
        let mut t = vsock();
        assert_eq!(t.take_notify(), None);
        t.write(reg::QUEUE_NOTIFY, 4, 2);
        assert_eq!(t.take_notify(), Some(QueueNotify(2)));
        assert_eq!(t.take_notify(), None); // drained
    }

    #[test]
    fn interrupt_status_is_acked_by_writing_the_bits_back() {
        let mut t = vsock();
        // Simulate the device raising an interrupt (used-ring bit 0).
        t.interrupt_status = 0b01;
        assert_eq!(t.read(reg::INTERRUPT_STATUS, 4), 0b01);
        t.write(reg::INTERRUPT_ACK, 4, 0b01);
        assert_eq!(t.read(reg::INTERRUPT_STATUS, 4), 0);
    }

    #[test]
    fn config_space_exposes_guest_cid_little_endian() {
        let t = MmioTransport::new_vsock(0x0102_0304_0506_0708);
        // 8-byte read of the whole CID.
        assert_eq!(t.read(reg::CONFIG_SPACE, 8), 0x0102_0304_0506_0708);
        // Byte 0 is the LE low byte.
        assert_eq!(t.read(reg::CONFIG_SPACE, 1), 0x08);
        // 4-byte read of the low word.
        assert_eq!(t.read(reg::CONFIG_SPACE, 4), 0x0506_0708);
    }

    #[test]
    fn unknown_register_reads_zero_and_write_only_regs_read_zero() {
        let t = vsock();
        assert_eq!(t.read(0x0d0, 4), 0); // gap / unimplemented
        assert_eq!(t.read(reg::QUEUE_NOTIFY, 4), 0); // write-only
        assert_eq!(t.read(reg::DRIVER_FEATURES, 4), 0); // write-only
    }

    #[test]
    fn out_of_range_queue_selector_is_ignored_not_panicking() {
        let mut t = vsock();
        t.write(reg::QUEUE_SEL, 4, 99); // no such queue
        t.write(reg::QUEUE_NUM, 4, 16); // must not panic
        assert_eq!(t.read(reg::QUEUE_READY, 4), 0);
        assert!(t.queue(99).is_none());
    }
}
