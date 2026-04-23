//! `nanovm-control-plane` — REST server binary.
//!
//! Wires a `MockHypervisor` by default so this binary is runnable on any
//! machine without `/dev/kvm`.
//!
//! Environment:
//! - `NANOVM_CONTROL_PLANE_ADDR` — bind address (default `127.0.0.1:8080`).
//! - `NANOVM_API_TOKENS` — comma-separated bearer tokens. **Empty disables
//!   auth** and emits a `WARN` log line on startup.

#![forbid(unsafe_code)]

use std::sync::Arc;

use axum::Extension;
use control_plane::{router, ApiTokens, AppState};
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{info, warn};
use vm_mock::MockHypervisor;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let addr =
        std::env::var("NANOVM_CONTROL_PLANE_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

    let tokens = Arc::new(ApiTokens::from_env());
    if tokens.is_empty() {
        warn!(
            "NANOVM_API_TOKENS is empty — /v1/* is unauthenticated. \
             Set this env var to a comma-separated list of bearer tokens \
             before exposing this service to the network."
        );
    } else {
        info!(count = tokens.len(), "bearer-token auth enabled");
    }

    let hypervisor: Arc<dyn vm_core::Hypervisor> = Arc::new(MockHypervisor::new());
    let app = router()
        .layer(Extension(tokens))
        .with_state(AppState::new(hypervisor));

    let listener = TcpListener::bind(&addr).await?;
    info!(%addr, "nanovm-control-plane listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("shutdown signal received, draining");
}
