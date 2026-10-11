//! # virtio-net — host-side paravirtualised network device
//!
//! This crate is the **host-side implementation** of the virtio-net
//! paravirtualised network interface, used by every KVM-based microVM
//! (Firecracker, Cloud Hypervisor, QEMU, nanovm). Guest Linux kernels
//! have shipped the matching driver (`drivers/net/virtio_net.c`) since
//! 2008. We implement the host end so Spring Boot inside the guest can
//! open a TCP socket to the outside world.
//!
//! New reader? Start with the **terminology** box below — every
//! acronym and term in one place — then the **architecture diagram**,
//! then jump into the module you need.
//!
//! ## Terminology (every acronym in one place)
//!
//! | Term | Expanded | What it means |
//! |---|---|---|
//! | **virtio** | — | Industry-standard **paravirtualised** I/O protocol for guest ↔ host communication in KVM-based VMs. Guest kernels ship drivers (`virtio_net`, `virtio_blk`, `virtio_vsock`, …); hosts ship devices (that's us). Spec: *Virtio 1.3*. |
//! | **virtio-net** | — | The specific virtio device class for a network interface card. Spec: *§5.1*. |
//! | **paravirtualised** | — | Guest and host **cooperate** through a known protocol instead of the host emulating real NIC hardware byte-for-byte. Faster than full emulation, needs guest-driver support. |
//! | **microVM** | — | A VM stripped down to just what's needed to run one workload: tiny kernel, minimal rootfs, millisecond boot. Compared to a "full" VM (whole desktop OS) it's two orders of magnitude smaller and faster. |
//! | **KVM** | Kernel-based Virtual Machine | The Linux kernel's built-in hypervisor. Programs the CPU's hardware virtualisation (VT-x / AMD-V) through `/dev/kvm` ioctls. Our `vm-kvm` crate wraps those ioctls. |
//! | **TX** | Transmit | **Guest → host direction.** Guest Linux writes an Ethernet frame; we read it and give it to the backend (TAP, userspace stack, mock). |
//! | **RX** | Receive | **Host → guest direction.** Backend hands us an inbound Ethernet frame; we write it into the chain the guest driver pre-posted; guest reads. |
//! | **MMIO** | Memory-Mapped I/O | A memory address range the guest reads/writes which **traps into KVM** (`KVM_EXIT_MMIO`) and hands control to us. The device's register bank lives here. |
//! | **MMIO doorbell** | — | The specific MMIO register `QueueNotify` the guest writes to tell us "I just added work to queue N, come drain it." Every data-path round-trip starts with one of these writes. |
//! | **virtqueue (vq)** | — | A FIFO of buffer descriptors in **guest RAM shared with us**. One vq per direction: `vq[0] = RX`, `vq[1] = TX` for a minimum virtio-net. |
//! | **descriptor** | — | 16-byte record `{ addr: u64, len: u32, flags: u16, next: u16 }` that points at a buffer somewhere in guest RAM. Carries the **address**, not the data. |
//! | **descriptor table** | — | A flat array of descriptors in guest RAM. Size is a power of two (we advertise 256 max). The guest allocates it at device setup. |
//! | **descriptor chain** | — | A linked list through the descriptor table via each descriptor's `next` field, terminated when `flags & DESC_F_NEXT == 0`. One chain = one logical message. For virtio-net it carries the net header + Ethernet frame. |
//! | **avail ring** | available ring | Circular queue in guest RAM. **Guest writes** heads here; we read them. One slot = one `u16` descriptor-table index = the start of one chain the guest has prepared for us. |
//! | **used ring** | used ring | Circular queue in guest RAM. **We write** heads here; guest reads them. One slot = `{ head: u32, written_len: u32 }` — the chain we just finished, plus how many bytes we wrote into writable buffers (0 on TX, 12+frame.len() on RX). |
//! | **head** | — | Descriptor-table index of the first descriptor in a chain. Avail publishes it, used returns it. |
//! | **idx** | index | Producer counter at the top of each ring. Driver's `avail.idx` grows when it adds a head; our `used.idx` grows when we return one. The counter is `u16` and wraps. |
//! | **VRING interrupt** | — | IRQ bit we set in `InterruptStatus` to tell the guest "I just added used entries, come consume them." From the guest's POV an IRQ fires on the device's vector. |
//! | **virtio-net header** | — | 12-byte struct prefixed on every frame on the wire of a virtio-net queue (also called `struct virtio_net_hdr`). Carries offload hints the real NIC would set. We advertise no offload → it's all zeros, but still mandatory. See [`VirtioNetHdr`]. |
//! | **Ethernet frame** | — | The L2 (layer-2) unit: 14-byte header `{dst_mac[6], src_mac[6], ethertype[2]}` + up to 1500 bytes of payload (typically an IP packet). What TCP/IP hands up and down. |
//! | **MTU** | Maximum Transmission Unit | The largest Ethernet payload we accept — 1500 bytes standard. 14-byte L2 header + 1500 payload = 1514 bytes per frame on the wire. |
//! | **FCS** | Frame Check Sequence | 4-byte CRC Ethernet uses on physical wires. Virtual interfaces (TAP, virtio-net) **do not carry it** — the host kernel adds/strips it at the real NIC. So our frames are 1514 bytes max, not 1518. |
//! | **GSO / TSO** | Generic / TCP Segmentation Offload | Hardware feature where guest hands over one big TCP segment and the NIC splits it into MTU-sized pieces. We don't negotiate it; `gso_type = NONE` in every header. |
//! | **TAP** | — | Linux kernel virtual Ethernet interface. Open `/dev/net/tun` + `TUNSETIFF` ioctl → kernel creates a new `nvm-tap-N` interface visible to `ip link`. Any byte we write to the fd shows up on the interface as "from the wire"; any packet the kernel routes to the interface is readable from the fd. See [`TapDevice`]. |
//! | **CAP_NET_ADMIN** | — | Linux capability needed for `TUNSETIFF` + any bridge/iptables work. Multi-tenant hosts restrict it. passt/pasta / userspace TCP-stacks avoid needing it. |
//! | **MAC address** | Media Access Control | 6-byte L2 identifier of a network interface. We advertise one in the device config space; the guest driver uses it instead of generating a random one. |
//!
//! ## Where this crate sits in nanovm
//!
//! ```text
//!   ┌──────────────────────────────────────────────────────────────┐
//!   │ GUEST (Linux kernel inside the microVM)                      │
//!   │                                                              │
//!   │   Spring Boot app  ───►  Linux TCP/IP stack  ───►  virtio_net│
//!   │                                                        │     │
//!   └────────────────────────────────────────────────────────┼─────┘
//!                                              shared memory │ MMIO trap
//!                                                   (vq, desc│  tables)
//!   ┌────────────────────────────────────────────────────────┼─────┐
//!   │ HOST (nanovm process on Linux kernel)                  ▼     │
//!   │                                                              │
//!   │   crates/vm-kvm:  KVM ioctls, MMIO exit routing, guest RAM   │
//!   │        │                                                     │
//!   │        ▼                                                     │
//!   │   crates/virtio-net  ◄── THIS CRATE ──                      │
//!   │        │                                                     │
//!   │        ├─ MmioTransport  (register model, feature negot.)   │
//!   │        ├─ VirtioNetDevice  (owns transport + backend)       │
//!   │        ├─ queue consumer  (walks desc chains, moves bytes)  │
//!   │        │                                                     │
//!   │        ▼  dyn NetworkBackend                                 │
//!   │   TapDevice ──►  Linux TAP fd  ──► bridge ──► NAT ──► wire  │
//!   │   MockBackend (unit tests, any platform)                     │
//!   └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Modules in this crate
//!
//! | Module | What it holds | Terminology section |
//! |---|---|---|
//! | `header` | 12-byte `VirtioNetHdr` wire struct (parse / serialize) | See *virtio-net header* above |
//! | `mmio` | `MmioTransport` — register model, feature negotiation, queue config | See *MMIO*, *virtqueue*, *MMIO doorbell* |
//! | `device` | `VirtioNetDevice` — glues transport + backend | — |
//! | `queue` (lands with sub-PR #B.2) | TX drain / RX fill — walks desc chains, moves bytes | File has its own extended per-flow diagrams |
//! | `tap` (Linux only) | `TapDevice` — opens `/dev/net/tun`, wraps `TUNSETIFF` | See *TAP*, *CAP_NET_ADMIN* |
//! | `mock` | `MockBackend` — in-memory `NetworkBackend` for unit tests | — |
//!
//! ## Public surface
//!
//! ```text
//!   error:            crate::VirtioNetError    (#[from] for std::io, QueueError, HeaderError)
//!   trait:            NetworkBackend           (read_frame, write_frame, readiness_fd)
//!   Linux backend:    TapDevice                (cfg(target_os = "linux"))
//!   test backend:     MockBackend              (any platform)
//!   header:           VirtioNetHdr             (12-byte wire struct)
//!   device:           VirtioNetDevice          (owns MmioTransport + Arc<dyn NetworkBackend>)
//!   transport:        MmioTransport            (register model, two queues, config space)
//! ```

// `#![deny(unsafe_code)]` at the crate root marks unsafe as a hard
// error by default. Rust's `forbid` variant can't be overridden by
// child modules (that's its whole purpose); `deny` can, via a
// module-level `#![allow(unsafe_code)]`. `tap.rs` opts in — it
// wraps FFI to `libc::ioctl` — and every other module stays
// mechanically safe. Reviewers grep for `allow(unsafe_code)` to
// find every place unsafe touches the tree.
#![deny(unsafe_code)]
#![warn(missing_docs)]

/// Errors this crate can surface. Callers match on the variants they
/// care about; the `#[from]` attribute lets `std::io::Error` bubble
/// up via the `?` operator without an explicit `.map_err(…)`.
///
/// # Rust concept: `thiserror::Error`
///
/// This derive macro generates the boilerplate for
/// [`std::error::Error`]. Without it we'd hand-write:
///
/// ```ignore
/// impl std::fmt::Display for VirtioNetError { … }
/// impl std::error::Error for VirtioNetError { … }
/// impl From<std::io::Error> for VirtioNetError { … }
/// ```
///
/// ~30 lines that compile to identical code but scream "boilerplate".
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VirtioNetError {
    /// Underlying I/O error opening or operating on the TAP device.
    #[error("virtio-net I/O: {0}")]
    Io(#[from] std::io::Error),

    /// TAP interface name too long or contains invalid characters.
    /// Linux caps interface names at `IFNAMSIZ - 1 = 15` bytes.
    #[error("invalid TAP interface name {name:?}: {reason}")]
    InvalidIfName {
        /// The offending name the caller passed.
        name: String,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// Error from parsing / serializing the 12-byte virtio-net header.
    /// Produced by the queue consumer when a guest TX chain's leading
    /// bytes aren't a valid header.
    #[error("virtio-net header: {0}")]
    Header(#[from] header::HeaderError),

    /// Error from the shared split-virtqueue parser (bad descriptor
    /// chain, guest-memory bounds, oversized `len` field, …). Produced
    /// by the queue consumer while walking rings.
    #[error("virtqueue: {0}")]
    Queue(#[from] virtio_queue::QueueError),
}

/// Result specialised to this crate's error type. Keeps signatures
/// short: `Result<T>` reads better than `Result<T, VirtioNetError>`.
pub type Result<T> = std::result::Result<T, VirtioNetError>;

/// Trait every network-transport backend implements. The virtio
/// device from sub-PR #B holds a `Box<dyn NetworkBackend>` so it
/// can be constructed with a real TAP in production and a mock
/// backend in unit tests.
///
/// # Rust concept: traits
///
/// A trait is Rust's version of an interface (Java) or a protocol
/// (Swift). Types opt in by writing `impl NetworkBackend for MyType`
/// and providing the required methods. Callers that want polymorphism
/// take `&dyn NetworkBackend` (dynamic dispatch, one v-table lookup
/// per call) or `impl NetworkBackend` (monomorphised, zero overhead
/// but different code emitted per concrete type).
///
/// We use `dyn NetworkBackend` here because the virtio device
/// swaps its backend at runtime (mock in tests, TAP in prod), and
/// the once-per-frame v-table cost is negligible next to a syscall.
///
/// # Threading model
///
/// Methods take `&self`, not `&mut self`. Implementations that need
/// interior mutability (e.g. TAP's fd is read+written concurrently
/// from the TX and RX threads) manage it inside — usually via a
/// synchronised primitive or the fact that the underlying syscalls
/// are themselves atomic on the kernel side.
pub trait NetworkBackend: Send + Sync + std::fmt::Debug {
    /// Read one Ethernet frame from the backend into `buf`.
    ///
    /// Returns the number of bytes actually written to `buf`. `0`
    /// means "no frame available right now" (non-blocking mode).
    /// Callers loop or `epoll` on the backend's readiness fd —
    /// exposed via [`NetworkBackend::readiness_fd`] on Linux.
    fn read_frame(&self, buf: &mut [u8]) -> Result<usize>;

    /// Write one Ethernet frame to the backend. Bytes in `frame`
    /// include the full 14-byte Ethernet header. Returns after the
    /// kernel accepts the frame — for a TAP backend that means it's
    /// been enqueued to the bridge/routing stack; delivery beyond
    /// that is out of the backend's hands.
    fn write_frame(&self, frame: &[u8]) -> Result<()>;

    /// Optional readiness fd for `epoll` / `poll` integration. Only
    /// meaningful when the backend is fd-backed (TAP). Returns
    /// `None` for in-memory backends (mocks), which the caller
    /// polls in a different way (e.g. a `Condvar`).
    ///
    /// The `i32` is a raw file descriptor — Linux fds are integers.
    /// We don't use `RawFd` from `std::os::unix` in the signature
    /// because that type isn't defined on non-Unix targets, and this
    /// trait needs to compile on macOS + Windows for cross-platform
    /// unit tests.
    fn readiness_fd(&self) -> Option<i32> {
        None
    }
}

#[cfg(target_os = "linux")]
pub mod tap;
#[cfg(target_os = "linux")]
pub use tap::TapDevice;

pub mod mock;
pub use mock::MockBackend;

pub mod header;
pub use header::{HeaderError, VirtioNetHdr, VIRTIO_NET_HDR_LEN};

pub mod mmio;
pub use mmio::{
    MmioTransport, QueueConfig, QueueNotify, RX_QUEUE_INDEX, TX_QUEUE_INDEX, VIRTIO_ID_NET,
};

pub mod device;
pub use device::VirtioNetDevice;

pub mod queue;
pub use queue::{
    process_rx, process_tx, ProcessStats, QueueCursor, MAX_CHAIN_BYTES, MAX_FRAME_LEN,
};

#[cfg(feature = "smoltcp-backend")]
pub mod smoltcp_backend;
#[cfg(feature = "smoltcp-backend")]
pub use smoltcp_backend::{SmoltcpBackend, DEFAULT_GUEST_IPV4, DEFAULT_HOST_IPV4};

#[cfg(feature = "smoltcp-backend")]
pub mod tcp_proxy;
#[cfg(feature = "smoltcp-backend")]
pub use tcp_proxy::TcpProxy;
