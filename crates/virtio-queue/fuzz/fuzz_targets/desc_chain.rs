//! Fuzz target: descriptor table parsing + chain walking.
//!
//! Exercises [`parse_descriptor_table`] and [`DescriptorChain`] against
//! arbitrary byte inputs. The fuzzer is looking for panics, index-out-of-
//! bounds accesses, infinite loops, or other misbehaviour that the bounded
//! unit-test inputs might miss.
//!
//! Run with:
//! ```sh
//! cargo +nightly fuzz run desc_chain
//! ```
//!
//! Wire format: the fuzzer packs a small header in front of its payload so
//! it can control queue-size and head-index independently of the table bytes.
//!
//! ```text
//! [0]   qsize_selector  — 2-bit field selects qsize from {1, 2, 4, 8}
//! [1]   head_lo         \
//! [2]   head_hi          }- u16 little-endian head index for DescriptorChain
//! [3..] table_bytes      — fed into parse_descriptor_table
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;
use virtio_queue::{parse_descriptor_table, DescriptorChain};

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }

    // Pick a small queue size so the table stays manageable.
    let qsize: u16 = match data[0] & 0x3 {
        0 => 1,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let head = u16::from_le_bytes([data[1], data[2]]);
    let buf = &data[3..];

    // Property 1: parse_descriptor_table never panics on arbitrary input.
    let table_result = parse_descriptor_table(buf, qsize);

    // Property 2: if parsing succeeded, walking the chain never panics
    // and always terminates (the cycle bound in DescriptorChain guarantees
    // this, but we verify by consuming the iterator).
    if let Ok(table) = table_result {
        let _chain: Vec<_> = DescriptorChain::new(&table, head).collect();
    }
});
