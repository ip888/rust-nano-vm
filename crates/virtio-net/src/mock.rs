//! In-memory network backend for tests.
//!
//! [`MockBackend`] holds two `VecDeque<Vec<u8>>` — one for frames
//! the virtio-net device would receive from the network (RX side),
//! one for frames the device would transmit to the network (TX
//! side). Tests wire them however they like: producer scripts push
//! into RX, consumer scripts drain TX.
//!
//! Deliberately allocation-heavy — this is a test aid, not a
//! performance-sensitive path. Zero-copy comes in the real TAP
//! backend.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::{NetworkBackend, Result};

/// In-memory network backend. See module docs.
///
/// # Rust concept: `Mutex` for interior mutability
///
/// The [`NetworkBackend`] trait methods take `&self`, not `&mut self`
/// — the virtio device from sub-PR #B holds one `Box<dyn NetworkBackend>`
/// shared between its TX and RX loops. We need to mutate the deques
/// through a shared reference, which Rust normally forbids. `Mutex`
/// is the standard way to do that: it wraps its contents and only
/// hands out a mutable reference through `lock()`, which blocks
/// other threads until we drop the guard.
///
/// (A `RwLock` would be marginally faster for reader-heavy loads;
/// we don't bother because this is test code and the queues alternate
/// producer/consumer roles.)
#[derive(Debug, Default)]
pub struct MockBackend {
    // The two queues are kept behind separate mutexes so a stuck TX
    // reader doesn't hold up an RX writer. Frame ownership is by
    // move — pushing a `Vec<u8>` transfers ownership into the queue,
    // popping transfers it back out.
    rx: Mutex<VecDeque<Vec<u8>>>,
    tx: Mutex<VecDeque<Vec<u8>>>,
}

impl MockBackend {
    /// Create a new empty mock.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a frame onto the RX side. Tests use this to script
    /// packets "arriving from the network".
    pub fn inject_rx(&self, frame: Vec<u8>) {
        // `.lock()` returns a `LockResult<MutexGuard>`. The result
        // is `Err` only if another thread panicked while holding
        // the lock (the mutex is "poisoned"). In production code
        // we'd handle that; in a mock we just unwrap because a
        // poisoned mock in a test is a test bug anyway.
        self.rx.lock().unwrap().push_back(frame);
    }

    /// Pop the next TX frame the device wrote. Returns `None` if
    /// the device hasn't transmitted anything yet.
    pub fn pop_tx(&self) -> Option<Vec<u8>> {
        self.tx.lock().unwrap().pop_front()
    }

    /// How many frames are queued on RX and TX respectively. Test
    /// helper for asserting on queue state.
    pub fn queue_depths(&self) -> (usize, usize) {
        let rx = self.rx.lock().unwrap().len();
        let tx = self.tx.lock().unwrap().len();
        (rx, tx)
    }
}

impl NetworkBackend for MockBackend {
    fn read_frame(&self, buf: &mut [u8]) -> Result<usize> {
        let mut rx = self.rx.lock().unwrap();
        match rx.pop_front() {
            None => Ok(0),
            Some(frame) => {
                // Match Linux TAP semantics: a `read()` into a buffer
                // smaller than the next frame **truncates and consumes**
                // the frame; the trailing bytes are lost and the caller
                // sees whatever fit. This matches the behavior of
                // `read(/dev/net/tun)` as implemented by the kernel's
                // tun driver (see `tun_do_read` in `drivers/net/tun.c`).
                // The virtio device from sub-PR #B always sizes its
                // RX buffers at ≥ MTU, so truncation never happens in
                // practice; tests that pass an undersized buffer are
                // asserting the truncation contract explicitly.
                let n = frame.len().min(buf.len());
                buf[..n].copy_from_slice(&frame[..n]);
                Ok(n)
            }
        }
    }

    fn write_frame(&self, frame: &[u8]) -> Result<()> {
        // Clone into an owned Vec so the caller's borrow ends
        // immediately. In a real TAP backend the write goes to the
        // kernel and completes synchronously — no queue.
        self.tx.lock().unwrap().push_back(frame.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_single_frame() {
        let mock = MockBackend::new();
        // Inject an "arriving from the network" frame.
        let injected = b"\xff\xff\xff\xff\xff\xff\x00\x11\x22\x33\x44\x55\x08\x06".to_vec();
        mock.inject_rx(injected.clone());

        // Read it back through the trait method the virtio device
        // will use.
        let mut buf = [0u8; 2048];
        let n = mock.read_frame(&mut buf).unwrap();
        assert_eq!(&buf[..n], injected.as_slice());
    }

    #[test]
    fn read_returns_zero_when_empty() {
        let mock = MockBackend::new();
        let mut buf = [0u8; 64];
        assert_eq!(mock.read_frame(&mut buf).unwrap(), 0);
    }

    #[test]
    fn write_reaches_tx_queue() {
        let mock = MockBackend::new();
        let frame = b"hello ethernet".to_vec();
        mock.write_frame(&frame).unwrap();
        assert_eq!(mock.pop_tx().as_deref(), Some(&frame[..]));
        assert_eq!(mock.pop_tx(), None);
    }

    #[test]
    fn oversized_frame_is_truncated_and_consumed_like_tap() {
        // Linux TAP read() into an undersized buffer truncates the
        // frame and consumes it. Mock matches that contract.
        let mock = MockBackend::new();
        let big = vec![0xabu8; 1500];
        mock.inject_rx(big.clone());

        let mut small = [0u8; 64];
        let n = mock.read_frame(&mut small).unwrap();
        assert_eq!(n, 64, "mock truncates to buf.len()");
        assert_eq!(&small[..], &big[..64]);

        // Frame was consumed — no retry with a bigger buffer gets
        // the rest back. Next read sees empty queue.
        let mut big_buf = [0u8; 2048];
        let n2 = mock.read_frame(&mut big_buf).unwrap();
        assert_eq!(n2, 0, "frame was consumed on the truncating read");
    }
}
