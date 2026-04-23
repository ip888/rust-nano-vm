//! Control plane: REST API in front of a [`Hypervisor`] backend.
//!
//! Scope: **M6**. Ships the HTTP surface for sandbox lifecycle operations
//! (create, start, stop, snapshot, restore, destroy), plus bearer-token
//! auth guarding every `/v1/*` route. Quotas and metering are deferred to
//! follow-up PRs on top of this foundation.
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

mod api;
mod auth;
mod error;
mod routes;

pub use auth::ApiTokens;
pub use routes::{router, AppState};
