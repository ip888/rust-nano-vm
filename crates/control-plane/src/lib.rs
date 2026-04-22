//! Control plane: REST API in front of a [`Hypervisor`] backend.
//!
//! Scope: **M6**. This milestone ships the HTTP surface for sandbox
//! lifecycle operations (create, start, stop, snapshot, restore, destroy).
//! Auth, quotas, and metering are deferred to follow-up PRs on top of this
//! foundation.
//!
//! Typical wiring:
//!
//! ```no_run
//! use std::sync::Arc;
//! use control_plane::{router, AppState};
//! use vm_mock::MockHypervisor;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let hypervisor = Arc::new(MockHypervisor::new());
//! let state = AppState::new(hypervisor);
//! let app = router(state);
//!
//! let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
//! axum::serve(listener, app).await?;
//! # Ok(())
//! # }
//! ```
//!
//! The `nanovm-control-plane` binary wires a `MockHypervisor` for easy
//! integration smoke-testing; real deployments construct a backend from
//! `vm-kvm` (M1+) and pass it in the same way.
//!
//! [`Hypervisor`]: vm_core::Hypervisor

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod api;
mod error;
mod routes;

pub use routes::{router, AppState};
