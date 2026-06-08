//! In-guest agent that handles agent-sandbox-proto requests from the host.
//!
//! Built as a static `x86_64-unknown-linux-musl` binary. The agent logic
//! lives in the `[[bin]]` target (see `main.rs`); this library crate
//! exists to anchor the workspace entry.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
