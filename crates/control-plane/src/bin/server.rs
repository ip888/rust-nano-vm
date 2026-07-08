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
//! - `NANOVM_OWNERSHIP_STORE` — path to a SQLite file that records
//!   which org owns which VM / snapshot. Unset → in-memory only, and
//!   after any restart every VM/snapshot falls back to the default org
//!   (fine for single-tenant / mock deploys; catastrophic for
//!   multi-tenant SaaS — one customer's VM becomes reachable to every
//!   other). Requires the binary built with `--features sqlite`.
//!   Accepts either `sqlite:///data/nanovm.sqlite` or a bare
//!   `/data/nanovm.sqlite` path.
//! - `NANOVM_BACKEND` — Hypervisor backend to install. Values:
//!     - `mock` (default) — in-process `MockHypervisor`, no /dev/kvm
//!       needed. Right pick for CI, smoke tests, and the "single
//!       binary on Fly.io" deploy shape.
//!     - `fleet` — process-fleet backend (`nanovm-fleet`). Spawns
//!       one `nanovm-jailer` subprocess per VM and forwards
//!       hypervisor methods over IPC. Gives each VM its own cgroup
//!       and its own crash domain. Requires the jailer + worker
//!       binaries on disk; see `NANOVM_FLEET_*` env vars below.
//! - `NANOVM_FLEET_JAILER_BINARY` — absolute path to `nanovm-jailer`
//!   when `NANOVM_BACKEND=fleet`. Default `/usr/local/bin/nanovm-jailer`.
//! - `NANOVM_FLEET_VMM_CHILD_BINARY` — absolute path to
//!   `nanovm-vmm-child`. Default `/usr/local/bin/nanovm-vmm-child`.
//! - `NANOVM_FLEET_SOCKET_DIR` — directory the fleet creates per-VM
//!   Unix sockets in. Default `/var/run/nanovm`.
//! - `NANOVM_FLEET_WARM_POOL_SIZE` — pre-spawned idle workers the
//!   fleet keeps ready. `0` (default) disables; non-zero pre-pays
//!   the spawn cost so `create_vm` is a socket-pop.
//! - `NANOVM_FLEET_MEMORY_LIMIT_MIB` / `NANOVM_FLEET_CPU_QUOTA_PCT` —
//!   default per-VM caps the jailer writes into the cgroup. Unset =
//!   no cap on that controller.
//! - `NANOVM_FLEET_CGROUP_PARENT` — override the parent cgroup. Unset
//!   = use the control-plane's own cgroup (right under a systemd
//!   `Delegate=memory cpu` unit).

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

    let (hypervisor, backend_label) = build_hypervisor()?;
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

    // Persistent ownership map when NANOVM_OWNERSHIP_STORE is set;
    // otherwise stays in-memory (the default). Fail at startup rather
    // than serving one customer's VM to another after a redeploy.
    let ownership = control_plane::OwnershipMap::from_env()
        .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?;

    // Optional Stripe billing context. `None` when the `billing`
    // feature is off OR when any of STRIPE_SECRET_KEY /
    // STRIPE_BILLING_PORTAL_RETURN_URL / NANOVM_SIGNUP_TOKEN is unset.
    // The signup / billing_portal handlers return 503 `billing_disabled`
    // in that case.
    #[cfg(feature = "billing")]
    let billing_ctx = build_billing_ctx();
    #[cfg(feature = "billing")]
    if billing_ctx.is_some() {
        info!("billing enabled: POST /v1/signup + GET /v1/billing/portal live");
    } else {
        info!("billing disabled (set STRIPE_SECRET_KEY + STRIPE_BILLING_PORTAL_RETURN_URL + NANOVM_SIGNUP_TOKEN to enable)");
    }

    let state = AppState::new(hypervisor)
        .with_warm_pool(warm_pool)
        .with_snapshot_store(snapshot_store)
        .with_backend_label(backend_label)
        .with_ownership_map(Arc::new(ownership));
    #[cfg(feature = "billing")]
    let state = state.with_billing(billing_ctx);

    let app = router()
        .layer(Extension(tokens))
        .layer(Extension(audit))
        .with_state(state);

    let listener = TcpListener::bind(&addr).await?;
    info!(%addr, "nanovm-control-plane listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// `(Hypervisor handle, static backend label)` — the build_hypervisor
/// return shape. Aliased to keep the function signature within
/// clippy's type-complexity threshold.
type BackendChoice = (Arc<dyn vm_core::Hypervisor>, &'static str);

/// Pick a Hypervisor backend based on `NANOVM_BACKEND`.
/// Returns the trait object + the static label used by
/// `GET /v1/health` and the Prometheus exposition.
///
/// - `mock` (default): in-process [`MockHypervisor`]. No /dev/kvm
///   needed; ships in every binary.
/// - `fleet`: [`nanovm_fleet::ProcessFleet`] driven by
///   `NANOVM_FLEET_*` env vars. Spawns one
///   `nanovm-jailer` per VM and forwards methods over IPC.
fn build_hypervisor() -> Result<BackendChoice, Box<dyn std::error::Error>> {
    let backend = std::env::var("NANOVM_BACKEND").unwrap_or_else(|_| "mock".to_string());
    let (hv, default_label): (std::sync::Arc<dyn vm_core::Hypervisor>, &'static str) =
        match backend.as_str() {
            "" | "mock" => {
                info!("backend: mock (in-process MockHypervisor)");
                (std::sync::Arc::new(MockHypervisor::new()), "mock")
            }
            "fleet" => {
                let cfg = fleet_config_from_env();
                info!(
                    jailer = %cfg.jailer_binary.display(),
                    worker = %cfg.vmm_child_binary.display(),
                    socket_dir = %cfg.socket_dir.display(),
                    warm_pool = cfg.warm_pool_size,
                    memory_mib = ?cfg.default_memory_limit_mib,
                    cpu_quota_pct = ?cfg.default_cpu_quota_pct,
                    "backend: fleet (process-fleet via nanovm-jailer)"
                );
                let fleet = nanovm_fleet::ProcessFleet::new(cfg)
                    .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?;
                (std::sync::Arc::new(fleet), "fleet")
            }
            other => {
                return Err(format!(
                    "NANOVM_BACKEND={other:?} is not recognised; valid values: mock, fleet"
                )
                .into());
            }
        };
    // Optional operator override for the label surfaced on `/v1/health`
    // and prometheus `nanovm_up`. Lets `Dockerfile.kvm` advertise itself
    // as `"kvm-fleet"` even though the backend picker only distinguishes
    // `"mock"` / `"fleet"` internally.
    //
    // Leaking a small startup-once String → `&'static str` is fine here:
    // AppState.backend_label needs `&'static str`, and this label lives
    // for the entire process anyway.
    let label: &'static str = match std::env::var("NANOVM_BACKEND_LABEL") {
        Ok(s) if !s.is_empty() => Box::leak(s.into_boxed_str()),
        _ => default_label,
    };
    Ok((hv, label))
}

/// Translate `NANOVM_FLEET_*` env vars into a [`FleetConfig`].
/// Missing values fall back to the [`FleetConfig::default`] shape.
fn fleet_config_from_env() -> nanovm_fleet::FleetConfig {
    use std::path::PathBuf;
    let mut cfg = nanovm_fleet::FleetConfig::default();
    if let Ok(p) = std::env::var("NANOVM_FLEET_JAILER_BINARY") {
        if !p.is_empty() {
            cfg.jailer_binary = PathBuf::from(p);
        }
    }
    if let Ok(p) = std::env::var("NANOVM_FLEET_VMM_CHILD_BINARY") {
        if !p.is_empty() {
            cfg.vmm_child_binary = PathBuf::from(p);
        }
    }
    if let Ok(p) = std::env::var("NANOVM_FLEET_SOCKET_DIR") {
        if !p.is_empty() {
            cfg.socket_dir = PathBuf::from(p);
        }
    }
    if let Ok(n) = std::env::var("NANOVM_FLEET_WARM_POOL_SIZE") {
        if let Ok(n) = n.parse::<usize>() {
            cfg.warm_pool_size = n;
        }
    }
    if let Ok(n) = std::env::var("NANOVM_FLEET_MEMORY_LIMIT_MIB") {
        if let Ok(n) = n.parse::<u64>() {
            cfg.default_memory_limit_mib = Some(n);
        }
    }
    if let Ok(n) = std::env::var("NANOVM_FLEET_CPU_QUOTA_PCT") {
        if let Ok(n) = n.parse::<u32>() {
            cfg.default_cpu_quota_pct = Some(n);
        }
    }
    if let Ok(p) = std::env::var("NANOVM_FLEET_CGROUP_PARENT") {
        if !p.is_empty() {
            cfg.cgroup_parent = Some(PathBuf::from(p));
        }
    }
    cfg
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

/// Compose a [`control_plane::billing::BillingCtx`] from env when all
/// three Stripe vars are set. Returns `None` when billing is not fully
/// configured — the handlers 503 with `billing_disabled` on requests.
///
/// Billing state persists into the same SQLite file the ownership store
/// uses. When `NANOVM_OWNERSHIP_STORE` is unset, falls back to
/// `InMemoryBillingStore` with a loud warning — fine for tests and demos,
/// catastrophic for prod SaaS.
#[cfg(feature = "billing")]
fn build_billing_ctx() -> Option<control_plane::billing::BillingCtx> {
    let cfg = control_plane::billing::BillingConfig::from_env()?;
    let store: std::sync::Arc<dyn control_plane::billing::BillingStore> =
        match std::env::var("NANOVM_OWNERSHIP_STORE")
            .ok()
            .filter(|s| !s.is_empty())
        {
            Some(spec) => {
                let path = spec.strip_prefix("sqlite://").unwrap_or(&spec);
                match control_plane::billing::SqliteBillingStore::open(path) {
                    Ok(s) => std::sync::Arc::new(s),
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "billing: SqliteBillingStore::open failed; falling back to in-memory"
                        );
                        std::sync::Arc::new(control_plane::billing::InMemoryBillingStore::default())
                    }
                }
            }
            None => {
                tracing::warn!(
                    "billing: NANOVM_OWNERSHIP_STORE unset; using InMemoryBillingStore. \
                 Stripe customer ids will be lost on restart — DO NOT use in prod."
                );
                std::sync::Arc::new(control_plane::billing::InMemoryBillingStore::default())
            }
        };
    let stripe = std::sync::Arc::new(control_plane::billing::StripeClient::new(
        cfg.stripe_secret_key.clone(),
    ));
    Some(control_plane::billing::BillingCtx {
        config: cfg,
        store,
        stripe,
    })
}
