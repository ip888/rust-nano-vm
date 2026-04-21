//! Control plane: axum REST/gRPC API, auth, quotas, per-second metering.
//!
//! Scope: **M6**. Placeholder so the workspace compiles. Real implementation
//! in M6 will expose `/v1/sandboxes` and friends, backed by a `Hypervisor`
//! implementation plus a pool of warm snapshots.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
