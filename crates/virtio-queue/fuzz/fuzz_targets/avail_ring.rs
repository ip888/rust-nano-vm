//! Fuzz target: available ring parsing and iteration.
//!
//! Exercises [`AvailRing::new`] and all of its accessor methods against
//! arbitrary byte inputs. Verifies that no input causes a panic or
//! out-of-bounds access.
//!
//! Run with:
//! ```sh
//! cargo +nightly fuzz run avail_ring
//! ```
//!
//! Input layout:
//! ```text
//! [0]    qsize_selector — 2-bit field selects qsize from {1, 2, 4, 8}
//! [1..2] last_seen_lo/hi — u16 LE used as the `last_seen` argument to iter_new
//! [3..]  ring_bytes — passed to AvailRing::new
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;
use virtio_queue::AvailRing;

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }

    let qsize: u16 = match data[0] & 0x3 {
        0 => 1,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let last_seen = u16::from_le_bytes([data[1], data[2]]);
    let buf = &data[3..];

    // Property 1: AvailRing::new never panics.
    if let Ok(ring) = AvailRing::new(buf, qsize) {
        // Property 2: all accessor methods are panic-free on a valid ring.
        let _ = ring.flags();
        let _ = ring.idx();
        let _ = ring.used_event();
        let _ = ring.qsize();

        // head() uses `slot % qsize`, so any slot value is safe.
        for slot in 0u16..qsize {
            let _ = ring.head(slot);
        }

        // Property 3: iter_new terminates regardless of last_seen / idx wrapping.
        let _heads: Vec<u16> = ring.iter_new(last_seen).collect();
    }
});
