//! Fuzz target: used ring read and write operations.
//!
//! Exercises [`UsedRing::new`] and all of its accessor and mutator methods
//! against arbitrary byte inputs. Verifies that no input causes a panic,
//! out-of-bounds write, or arithmetic overflow.
//!
//! Run with:
//! ```sh
//! cargo +nightly fuzz run used_ring
//! ```
//!
//! Input layout:
//! ```text
//! [0]    qsize_selector — 2-bit selects qsize from {1, 2, 4, 8}
//! [1..4] head_idx (u32 LE) — used as the `head_idx` arg to push()
//! [5..8] written_len (u32 LE) — used as the `written_len` arg to push()
//! [9..]  ring_bytes — passed to UsedRing::new
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;
use virtio_queue::UsedRing;

fuzz_target!(|data: &[u8]| {
    if data.len() < 9 {
        return;
    }

    let qsize: u16 = match data[0] & 0x3 {
        0 => 1,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let head_idx = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
    let written_len = u32::from_le_bytes([data[5], data[6], data[7], data[8]]);
    let buf = &data[9..];

    // Property 1: UsedRing::new never panics.
    // We work on a copy so push() mutations don't alias the fuzz input.
    let mut owned: Vec<u8> = buf.to_vec();
    if let Ok(mut ring) = UsedRing::new(&mut owned, qsize) {
        // Property 2: accessor methods are panic-free.
        let _ = ring.flags();
        let _ = ring.idx();
        let _ = ring.avail_event();
        let _ = ring.qsize();

        for slot in 0u16..qsize {
            let _ = ring.elem(slot);
        }

        // Property 3: push() doesn't panic or overflow even at the
        // u16::MAX idx boundary.
        let _ = ring.push(head_idx, written_len);
    }
});
