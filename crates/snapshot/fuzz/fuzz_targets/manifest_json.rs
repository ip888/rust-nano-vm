//! Fuzz target: snapshot manifest JSON parsing.
//!
//! Exercises [`Manifest::from_json`] against arbitrary byte inputs.
//! The JSON parser must never panic, even on inputs that are not valid
//! UTF-8, not valid JSON, or valid JSON but with unexpected types.
//!
//! Run with:
//! ```sh
//! cargo +nightly fuzz run manifest_json
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;
use snapshot::Manifest;

fuzz_target!(|data: &[u8]| {
    // Property: from_json never panics on any byte sequence. Errors are
    // expected and fine; panics are bugs.
    let _ = Manifest::from_json(data);
});
