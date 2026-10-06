//! # Host-side virtio-net device skeleton
//!
//! > **Terminology:** every term in this file (MMIO, virtqueue,
//! > NetworkBackend, TAP, dyn trait object, …) is defined once in
//! > the crate-root terminology table — see the top of `lib.rs`.
//!
//! This is the piece sub-PR #C will register with `crates/vm-kvm` so
//! that guest **MMIO** (Memory-Mapped I/O) exits in the device's
//! register window route into [`VirtioNetDevice::mmio_read`] /
//! [`VirtioNetDevice::mmio_write`]. The device **composes two
//! lower-level pieces** into one object the KVM backend can hold:
//!
//! ```text
//!    VirtioNetDevice
//!    ├── MmioTransport     ← registers, feature negotiation, queue config
//!    │                       (the "control plane" the guest driver pokes)
//!    │
//!    └── Arc<dyn NetworkBackend>  ← actual Ethernet frame transport
//!        │                           (the "data plane")
//!        │
//!        ├── TapDevice (production)   ← /dev/net/tun fd + bridge
//!        └── MockBackend (tests)       ← VecDeque<Vec<u8>>
//! ```
//!
//! - a [`MmioTransport`](crate::MmioTransport) — the register model
//!   the guest driver pokes to discover the device and program the
//!   virtqueues;
//! - a [`NetworkBackend`](crate::NetworkBackend) — the transport that
//!   actually moves Ethernet frames (TAP in production, mock in tests).
//!
//! What this sub-PR deliberately **doesn't** do: walk descriptor
//! chains, read/write guest RAM, or drive the virtio-net header on/off
//! frames. That lives in sub-PR #B.2 once the device is wired into
//! vm-kvm's `GuestMemory`-style accessor. The scaffolding here lets us
//! land feature negotiation + register plumbing + the Box<dyn Backend>
//! seam separately so each PR is reviewable at a sitting.
//!
//! # Rust concept: `Box<dyn NetworkBackend>`
//!
//! The device needs to be constructible with either a real `TapDevice`
//! (production) or a `MockBackend` (unit tests + sub-PR #C's integration
//! tests). The two concrete types have the same methods but different
//! sizes and internals, so we can't store them as a plain field.
//!
//! `Box<dyn NetworkBackend>` solves this: `Box` is a heap allocation,
//! `dyn NetworkBackend` is a *trait object* — a fat pointer carrying
//! (1) a pointer to the backend's data and (2) a v-table pointer that
//! names the concrete type's `read_frame` / `write_frame` /
//! `readiness_fd`. Every call through the backend costs one v-table
//! lookup; next to a syscall or a KVM exit that's rounding error.
//!
//! Alternative we rejected: making `VirtioNetDevice` generic over the
//! backend type (`VirtioNetDevice<B: NetworkBackend>`). That gives
//! zero-cost dispatch via monomorphisation but forces every caller
//! (including `vm-kvm`) to be generic too. The dynamic-dispatch
//! trade-off keeps the public API narrow.

use std::sync::Arc;

use crate::{MmioTransport, NetworkBackend, QueueNotify};

/// A host-side virtio-net device wired to a [`NetworkBackend`].
///
/// Construct with [`VirtioNetDevice::new`] (owning backend) or
/// [`VirtioNetDevice::with_backend_arc`] (shared backend) and hand to
/// `vm-kvm`'s MMIO exit handler. Reads are `&self`; writes are
/// `&mut self`.
#[derive(Debug)]
pub struct VirtioNetDevice {
    transport: MmioTransport,
    backend: Arc<dyn NetworkBackend>,
    mac: [u8; 6],
}

impl VirtioNetDevice {
    /// Build a device around `backend`, advertising `mac` to the guest.
    ///
    /// The backend is wrapped in `Arc<dyn NetworkBackend>` so other
    /// components (e.g. a readiness-fd poll loop in `vm-kvm`) can hold
    /// a reference without having to lock the whole device. The
    /// `Box<dyn NetworkBackend>` the caller is likely to have gets
    /// adapted automatically because `Box<T>` can be converted into
    /// `Arc<T>` via its `From` impl.
    pub fn new(backend: Box<dyn NetworkBackend>, mac: [u8; 6]) -> Self {
        Self::with_backend_arc(Arc::from(backend), mac)
    }

    /// Build a device around a shared backend. Prefer [`new`](Self::new)
    /// unless you specifically need `Arc` semantics on the caller side.
    pub fn with_backend_arc(backend: Arc<dyn NetworkBackend>, mac: [u8; 6]) -> Self {
        Self {
            transport: MmioTransport::new_net(mac),
            backend,
            mac,
        }
    }

    /// MAC address advertised in the device's config space.
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// Borrow the underlying transport (status, features, queue config).
    pub fn transport(&self) -> &MmioTransport {
        &self.transport
    }

    /// Borrow the backend (shared `Arc`). Returned as a `&dyn
    /// NetworkBackend` because callers never need the `Arc`-ness —
    /// they just want to call methods through the trait.
    pub fn backend(&self) -> &dyn NetworkBackend {
        &*self.backend
    }

    /// Clone the backend handle (shared ownership). Useful when
    /// `vm-kvm` wants a copy to park on an epoll loop while the
    /// device itself stays locked by the MMIO handler.
    pub fn backend_arc(&self) -> Arc<dyn NetworkBackend> {
        Arc::clone(&self.backend)
    }

    /// `true` once the driver has finished bringing the device up
    /// (`DRIVER_OK` set in the status register).
    pub fn driver_ok(&self) -> bool {
        self.transport.driver_ok()
    }

    /// Features negotiated with the driver. Masked by the device's own
    /// offerings — the driver cannot enable a feature we didn't
    /// advertise.
    pub fn negotiated_features(&self) -> u64 {
        self.transport.negotiated_features()
    }

    /// `true` while the device's IRQ line is asserted (the guest
    /// hasn't yet acknowledged the last used-buffer or config-change
    /// notification).
    pub fn interrupt_asserted(&self) -> bool {
        self.transport.interrupt_asserted()
    }

    /// Handle an MMIO read within the device register window.
    pub fn mmio_read(&self, offset: u64, size: usize) -> u64 {
        self.transport.read(offset, size)
    }

    /// Handle an MMIO write within the device register window.
    ///
    /// Returns the queue index if the write was a `QueueNotify` kick,
    /// so the caller (sub-PR #B.2's queue consumer) knows to drain
    /// that queue.
    pub fn mmio_write(&mut self, offset: u64, size: usize, value: u64) -> Option<u32> {
        self.transport.write(offset, size, value);
        self.transport.take_notify().map(|QueueNotify(q)| q)
    }

    /// Assert the device's used-buffer interrupt. Call this after a
    /// successful [`crate::process_tx`] / [`crate::process_rx`] has
    /// pushed one or more completed chains back onto the used ring,
    /// so the guest driver reads the ring and reclaims its buffers.
    ///
    /// Pass-through to `MmioTransport::raise_vring_interrupt` — kept
    /// as a method on the device so callers don't need the internal
    /// transport reference (`transport()` returns `&`, not `&mut`).
    pub fn raise_vring_interrupt(&mut self) {
        self.transport.raise_vring_interrupt();
    }

    /// Current value of the device status register. Composed of
    /// `STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK |
    /// STATUS_DRIVER_OK` bits as the guest driver walks the
    /// bring-up sequence. Non-zero with `ACKNOWLEDGE` set proves
    /// the guest driver has probed and recognised the device.
    pub fn status(&self) -> u32 {
        self.transport.status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::{
        RX_QUEUE_INDEX, STATUS_ACKNOWLEDGE, STATUS_DRIVER, STATUS_DRIVER_OK, STATUS_FEATURES_OK,
        TX_QUEUE_INDEX, VIRTIO_F_VERSION_1, VIRTIO_NET_F_MAC, VIRTIO_NET_F_STATUS,
    };
    use crate::MockBackend;

    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

    // Register offsets, locally mirrored for test clarity.
    const REG_DEVICE_ID: u64 = 0x008;
    const REG_DEVICE_FEATURES: u64 = 0x010;
    const REG_DEVICE_FEATURES_SEL: u64 = 0x014;
    const REG_DRIVER_FEATURES: u64 = 0x020;
    const REG_DRIVER_FEATURES_SEL: u64 = 0x024;
    const REG_QUEUE_NOTIFY: u64 = 0x050;
    const REG_STATUS: u64 = 0x070;
    const REG_CONFIG_SPACE: u64 = 0x100;

    fn device() -> VirtioNetDevice {
        VirtioNetDevice::new(Box::new(MockBackend::new()), MAC)
    }

    /// Variant that lets the test keep a typed `Arc<MockBackend>` for
    /// inspection. Needed because `NetworkBackend` doesn't expose an
    /// `Any`-style downcast — adding one just to peek inside tests
    /// would widen the trait for no production benefit.
    fn device_with_mock() -> (VirtioNetDevice, Arc<MockBackend>) {
        let mock: Arc<MockBackend> = Arc::new(MockBackend::new());
        let backend: Arc<dyn NetworkBackend> = Arc::clone(&mock) as Arc<dyn NetworkBackend>;
        let dev = VirtioNetDevice::with_backend_arc(backend, MAC);
        (dev, mock)
    }

    #[test]
    fn device_id_is_virtio_net() {
        let d = device();
        assert_eq!(d.mmio_read(REG_DEVICE_ID, 4) as u32, 1);
    }

    #[test]
    fn mac_round_trips_through_config_space() {
        let d = device();
        for (i, byte) in MAC.iter().enumerate() {
            let v = d.mmio_read(REG_CONFIG_SPACE + i as u64, 1) as u8;
            assert_eq!(v, *byte);
        }
        assert_eq!(d.mac(), MAC);
    }

    #[test]
    fn driver_ok_tracks_the_status_register() {
        let mut d = device();
        assert!(!d.driver_ok());
        let bits = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK;
        d.mmio_write(REG_STATUS, 4, u64::from(bits));
        assert!(d.driver_ok());
    }

    #[test]
    fn negotiated_features_round_trip_through_mmio() {
        let mut d = device();
        // Driver accepts the three we advertise.
        d.mmio_write(REG_DRIVER_FEATURES_SEL, 4, 0);
        d.mmio_write(REG_DRIVER_FEATURES, 4, (1 << 5) | (1 << 16));
        d.mmio_write(REG_DRIVER_FEATURES_SEL, 4, 1);
        d.mmio_write(REG_DRIVER_FEATURES, 4, 1); // bit 32
        assert_eq!(
            d.negotiated_features(),
            VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS,
        );
    }

    #[test]
    fn mmio_write_returns_queue_index_on_notify() {
        let mut d = device();
        assert_eq!(
            d.mmio_write(REG_STATUS, 4, u64::from(STATUS_ACKNOWLEDGE)),
            None
        );
        let kick = d.mmio_write(REG_QUEUE_NOTIFY, 4, u64::from(TX_QUEUE_INDEX));
        assert_eq!(kick, Some(TX_QUEUE_INDEX));
        let kick_rx = d.mmio_write(REG_QUEUE_NOTIFY, 4, u64::from(RX_QUEUE_INDEX));
        assert_eq!(kick_rx, Some(RX_QUEUE_INDEX));
    }

    #[test]
    fn mmio_read_touches_no_backend_state() {
        // Discovery and config reads must not disturb the backend —
        // the guest driver reads these many times during probe.
        let (d, mock) = device_with_mock();
        let depths0 = mock.queue_depths();
        let _ = d.mmio_read(REG_DEVICE_ID, 4);
        let _ = d.mmio_read(REG_CONFIG_SPACE, 4);
        let _ = d.mmio_read(REG_DEVICE_FEATURES, 4);
        let _ = d.mmio_read(REG_DEVICE_FEATURES_SEL, 4);
        assert_eq!(mock.queue_depths(), depths0);
    }

    #[test]
    fn backend_arc_clones_share_the_same_underlying_backend() {
        let d = device();
        let a1 = d.backend_arc();
        let a2 = d.backend_arc();
        // Two Arc clones must point at the same allocation.
        assert!(Arc::ptr_eq(&a1, &a2));
    }
}
