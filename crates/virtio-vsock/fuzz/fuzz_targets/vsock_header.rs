//! Fuzz target: virtio-vsock packet header parsing.
//!
//! Exercises [`VsockHeader::from_bytes`] against arbitrary byte inputs.
//! When parsing succeeds, also verifies the roundtrip property: serialising
//! back via [`VsockHeader::write_to`] and re-parsing must yield the same
//! value.
//!
//! Run with:
//! ```sh
//! cargo +nightly fuzz run vsock_header
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;
use virtio_vsock::{VsockHeader, VSOCK_HDR_LEN};

fuzz_target!(|data: &[u8]| {
    // Property 1: from_bytes never panics on any input.
    let Ok(hdr) = VsockHeader::from_bytes(data) else {
        return;
    };

    // Property 2: roundtrip — serialise then re-parse must produce the
    // same value. A failure here indicates that write_to and from_bytes
    // have diverged (e.g. different field offsets).
    let bytes = hdr.to_bytes();
    let hdr2 = VsockHeader::from_bytes(&bytes)
        .expect("re-parsing a freshly serialised VsockHeader must succeed");
    assert_eq!(hdr, hdr2, "VsockHeader roundtrip mismatch");

    // Property 3: write_to into a larger buffer still works (the >= check
    // in write_to means extra bytes should be silently ignored after the
    // write position).
    let mut big_buf = vec![0u8; VSOCK_HDR_LEN + 16];
    hdr.write_to(&mut big_buf)
        .expect("write_to into oversized buffer must succeed");
    let hdr3 = VsockHeader::from_bytes(&big_buf)
        .expect("re-parsing from oversized buffer must succeed");
    assert_eq!(hdr, hdr3, "VsockHeader write_to/from_bytes with oversized buffer mismatch");
});
