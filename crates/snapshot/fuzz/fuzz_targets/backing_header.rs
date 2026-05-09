//! Fuzz target: snapshot backing-file header parsing.
//!
//! Exercises [`BackingFileHeader::from_bytes`] against arbitrary byte
//! inputs. When parsing succeeds, verifies the roundtrip property.
//!
//! Run with:
//! ```sh
//! cargo +nightly fuzz run backing_header
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;
use snapshot::{BackingFileHeader, BACKING_HDR_LEN};

fuzz_target!(|data: &[u8]| {
    // Property 1: from_bytes never panics on any input.
    let Ok(hdr) = BackingFileHeader::from_bytes(data) else {
        return;
    };

    // Property 2: roundtrip — a successfully parsed header must
    // serialise back without error and re-parse to the same value.
    let mut buf = [0u8; BACKING_HDR_LEN];
    hdr.write_to(&mut buf)
        .expect("write_to on a freshly parsed header must succeed");
    let hdr2 = BackingFileHeader::from_bytes(&buf)
        .expect("re-parsing a freshly serialised BackingFileHeader must succeed");
    assert_eq!(hdr, hdr2, "BackingFileHeader roundtrip mismatch");
});
