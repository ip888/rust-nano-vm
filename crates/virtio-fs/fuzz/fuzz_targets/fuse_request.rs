//! Fuzz target: full FUSE packet parsing via [`parse_request`].
//!
//! Exercises the top-level FUSE dispatcher against arbitrary byte slices.
//! This covers every opcode branch in `parse_request` — fixed-body structs,
//! variable-length name/data payloads, and the internal `split_nul` helper
//! — in a single target, letting the fuzzer discover opcode-specific edge
//! cases by mutating the opcode field in the synthesised `FuseInHeader`.
//!
//! Run with:
//! ```sh
//! cargo +nightly fuzz run fuse_request
//! ```
//!
//! Properties verified:
//!
//! 1. `parse_request` **never panics** on any input, regardless of length or
//!    content.
//! 2. If `parse_request` succeeds, **`dispatch`** with a no-op handler also
//!    never panics and returns either `Some(_)` or `None` (no-reply op).

#![no_main]

use libfuzzer_sys::fuzz_target;
use virtio_fs::dispatch::{dispatch, parse_request, FuseHandler};
use virtio_fs::FuseInHeader;

/// Minimal no-op handler: every method returns the default `ENOSYS`.
struct NullHandler;
impl FuseHandler for NullHandler {}

fuzz_target!(|data: &[u8]| {
    // Property 1: parse_request never panics.
    let Ok((hdr, req)) = parse_request(data) else {
        return;
    };

    // Property 2: dispatch on a valid parse never panics.
    let _ = dispatch(hdr, req, &mut NullHandler);
});
