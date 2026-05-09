//! Fuzz target: packed-ring descriptor array parsing.
//!
//! Exercises [`parse_packed_ring`] against arbitrary byte inputs. Verifies
//! that no input causes a panic or out-of-bounds access, and that the
//! accessor methods on a successfully-parsed [`PackedDesc`] are also
//! panic-free.
//!
//! Run with:
//! ```sh
//! cargo +nightly fuzz run packed_ring
//! ```
//!
//! Input layout:
//! ```text
//! [0]   qsize_selector — 2-bit field selects qsize from {1, 2, 4, 8}
//! [1..] ring_bytes     — passed to parse_packed_ring
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;
use virtio_queue::parse_packed_ring;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    // Pick a small queue size so the ring stays manageable.
    let qsize: u16 = match data[0] & 0x3 {
        0 => 1,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let buf = &data[1..];

    // Property 1: parse_packed_ring never panics on arbitrary input.
    if let Ok(ring) = parse_packed_ring(buf, qsize) {
        // Property 2: all accessor methods are panic-free on a valid ring.
        for desc in &ring {
            let _ = desc.has_next();
            let _ = desc.is_writable();
            let _ = desc.is_indirect();
            let _ = desc.is_avail();
            let _ = desc.is_used();
            let _ = desc.addr;
            let _ = desc.len;
            let _ = desc.id;
            let _ = desc.flags;
        }
    }
});
