//! Control plane: REST API in front of a [`Hypervisor`] backend.
//!
//! Ships the HTTP surface for sandbox lifecycle operations (create,
//! start, stop, snapshot, restore, destroy), bearer-token auth on every
//! `/v1/*` route, a per-token token-bucket quota on the `/fork` route,
//! per-caller usage metering, and a Prometheus `/metrics` endpoint.
//!
//! Typical wiring:
//!
//! ```no_run
//! use std::sync::Arc;
//! use axum::{Extension, Router};
//! use control_plane::{router, ApiTokens, AppState};
//! use vm_mock::MockHypervisor;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let hypervisor = Arc::new(MockHypervisor::new());
//! let tokens = Arc::new(ApiTokens::from_env()); // NANOVM_API_TOKENS env
//! let app: Router = router()
//!     .layer(Extension(tokens))
//!     .with_state(AppState::new(hypervisor));
//!
//! let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
//! axum::serve(listener, app).await?;
//! # Ok(())
//! # }
//! ```
//!
//! If `NANOVM_API_TOKENS` is empty the middleware short-circuits (auth
//! disabled) — useful for local development but **never** for a reachable
//! deployment. The binary logs a `WARN` line in that mode.
//!
//! The `nanovm-control-plane` binary wires a `MockHypervisor` for easy
//! integration smoke-testing; real deployments construct a backend from
//! `vm-kvm` (M1+) and pass it in the same way.
//!
//! [`Hypervisor`]: vm_core::Hypervisor

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// `serde_json::json!{...}` builds the whole OpenAPI document in a
// single literal — it needs more than the default 128 recursion
// budget once the schema has a couple-dozen entries.
#![recursion_limit = "256"]

mod api;
mod audit;
mod auth;
mod error;
mod exec_stream;
pub mod fork_quota;
pub mod metrics;
mod ownership;
mod request_id;
mod routes;
mod sandbox;
mod snapshot_export;
pub mod snapshot_store;
mod time;
pub mod warm_pool;

pub use api::openapi_spec;
pub use audit::AuditLog;
pub use auth::{ApiTokens, OrgId};
pub use fork_quota::ForkQuota;
pub use metrics::Metrics;
pub use request_id::RequestId;
pub use routes::{router, AppState};
pub use warm_pool::WarmPool;
