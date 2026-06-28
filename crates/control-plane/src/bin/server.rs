//! `nanovm-control-plane` — REST server binary.
//!
//! Wires a `MockHypervisor` by default so this binary is runnable on any
//! machine without `/dev/kvm`.
//!
//! Environment:
//! - `NANOVM_CONTROL_PLANE_ADDR` — bind address (default `127.0.0.1:8080`).
//! - `NANOVM_API_TOKENS` — comma-separated bearer tokens. **Empty disables
//!   auth** and emits a `WARN` log line on startup.
//! - `NANOVM_AUDIT_LOG` — when set to a filesystem path, mutating `/v1/*`
//!   calls are appended as JSON lines for compliance / forensics.
//! - `NANOVM_WARM_POOL_PER_SNAPSHOT` — target number of pre-restored VMs
//!   to keep ready per source snapshot. Default `0` (disabled).
//! - `NANOVM_SNAPSHOT_STORE` — durable snapshot store URI. Supported
//!   schemes: `file:///abs/path` (always available) and `s3://bucket[/prefix]`
//!   (requires `--features s3`). Unset → `/v1/snapshots/:id/export` and
//!   `/v1/snapshots/import` return 501.
//! - `NANOVM_S3_ENDPOINT` — custom S3 endpoint for MinIO / R2 / Wasabi.
//!   Read by the S3 backend when constructed.
//! - `NANOVM_LOG_FORMAT` — `text` (default) for human-friendly logs, or
//!   `json` for newline-delimited structured logs aimed at log
//!   aggregators (Loki / Datadog / CloudWatch / OpenSearch). Set to
//!   `json` on every reachable deployment.
//! - `NANOVM_TOKEN_STORE_PATH` — path to a JSON file the control plane
//!   uses to persist runtime-issued API keys (`POST /v1/keys`) across
//!   restarts. Unset → in-memory only (keys are lost on restart, fine
//!   for ephemeral dev). Set to a path on persistent storage for any
//!   production deployment where tenants self-serve their keys.

#![forbid(unsafe_code)]

use std::sync::Arc;

use axum::Extension;
use control_plane::{router, ApiTokens, AppState, AuditLog, WarmPool};
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{info, warn};
use vm_mock::MockHypervisor;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

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
    if let Ok(path) = std::env::var("NANOVM_TOKEN_STORE_PATH") {
        if !path.is_empty() {
            info!(path = %path, "runtime token persistence enabled");
        }
    } else {
        info!(
            "runtime token persistence disabled \
             (set NANOVM_TOKEN_STORE_PATH=/var/lib/nanovm/tokens.json to enable)"
        );
    }

    let audit = AuditLog::from_env();
    if let Some(path) = audit.path() {
        info!(path = %path.display(), "audit log appender enabled");
    }
    if !audit.is_disabled() && tokens.is_empty() {
        warn!(
            "NANOVM_AUDIT_LOG is set but NANOVM_API_TOKENS is empty; \
             audit lines will use the literal token \"anonymous\". \
             Configure tokens before relying on the audit log."
        );
    }

    let hypervisor: Arc<dyn vm_core::Hypervisor> = Arc::new(MockHypervisor::new());
    let warm_pool = WarmPool::from_env(Arc::clone(&hypervisor));
    if warm_pool.is_disabled() {
        info!("warm pool disabled (set NANOVM_WARM_POOL_PER_SNAPSHOT=N to enable)");
    } else {
        info!(per_snapshot = warm_pool.per_snapshot(), "warm pool enabled");
    }

    // Durable snapshot store (S3 / MinIO / R2 / local filesystem).
    // Failing at startup beats failing on the first export — a typo
    // in NANOVM_SNAPSHOT_STORE shouldn't surface as a customer 5xx.
    let snapshot_store = control_plane::snapshot_store::from_env()
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    match &snapshot_store {
        Some(s) => info!(uri = %s.display(), "durable snapshot store enabled"),
        None => info!("durable snapshot store disabled (set NANOVM_SNAPSHOT_STORE to enable)"),
    }

    let app = router()
        .layer(Extension(tokens))
        .layer(Extension(audit))
        .with_state(
            AppState::new(hypervisor)
                .with_warm_pool(warm_pool)
                .with_snapshot_store(snapshot_store),
        );

    let listener = TcpListener::bind(&addr).await?;
    info!(%addr, "nanovm-control-plane listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Bootstrap tracing. Honours `RUST_LOG` for level filtering (default
/// `info`) and `NANOVM_LOG_FORMAT` for shape (`text` default, `json` for
/// aggregator-friendly newline-delimited structured logs). Unknown
/// format values fall back to text and log a single WARN line so the
/// operator notices the typo.
fn init_logging() {
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let format = std::env::var("NANOVM_LOG_FORMAT").unwrap_or_default();
    match format.as_str() {
        "json" => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(false)
                .init();
        }
        "" | "text" => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
        other => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
            warn!(
                format = other,
                "unknown NANOVM_LOG_FORMAT; defaulting to text. \
                 Valid values: text, json"
            );
        }
    }
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
