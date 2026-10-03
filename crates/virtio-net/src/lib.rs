//! Host-side virtio-net paravirtualised network device.
//!
//! See the crate `README.md` for the architectural preface: what
//! virtio-net is, how TX/RX descriptor rings + the MMIO doorbell
//! work, and how this crate splits across four sub-PRs that
//! together give enterprise-Java guests real host-to-guest
//! networking.
//!
//! # Public surface (sub-PR #A — scaffold)
//!
//! ```text
//! error:            crate::VirtioNetError
//! trait:            NetworkBackend
//! Linux backend:    TapDevice   (behind cfg(target_os = "linux"))
//! test backend:     MockBackend (any platform)
//! ```
//!
//! Sub-PR #B will add `VirtioNetDevice` on top of this trait — the
//! actual virtio-mmio state machine that talks to the guest's
//! virtio-net driver.

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
