//! Library surface for the `nanovm-mcp` binary.
//!
//! The crate ships as a single binary (`src/main.rs`), but the
//! protocol, tool-dispatch, and HTTP-client modules are also exposed
//! here so integration tests under `tests/` can exercise them
//! without spawning the binary.

#![forbid(unsafe_code)]

pub mod client;
pub mod mcp;
pub mod tools;
