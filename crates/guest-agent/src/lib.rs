//! In-guest agent that handles agent-sandbox-proto requests from the host.
//!
//! Scope: **M2**. Ships as a static `x86_64-unknown-linux-musl` binary.
//! Placeholder so the workspace compiles — in M2 this crate will grow a
//! `[[bin]]` target and speak virtio-vsock.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
