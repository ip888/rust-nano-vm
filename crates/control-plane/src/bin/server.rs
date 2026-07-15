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
//! - `STRIPE_SECRET_KEY` / `STRIPE_BILLING_PORTAL_RETURN_URL` /
//!   `NANOVM_SIGNUP_TOKEN` — Stripe billing credentials. When all
//!   three are set (and the binary is built `--features billing`),
//!   `POST /v1/signup` and `GET /v1/billing/portal` go live. Never
//!   commit; wire via `flyctl secrets set` / Helm value / K8s Secret.
//! - `NANOVM_MARKETPLACE_CONFIG` — path to a JSON file defining the
//!   curated snapshot marketplace (see `deploy/marketplace/example.json`).
//!   Unset → the public `GET /v1/marketplace/snapshots` endpoint
//!   returns `{"snapshots": []}`. Read once at startup.
//! - `NANOVM_PLAN_TIERS` — map Stripe price ids to named tiers +
//!   fork RPS. Format:
//!   `price_ABC=free:5,price_XYZ=pro:100,price_ENT=enterprise:1000`.
//!   Read at startup; drives `GET /v1/billing/plan` and, since #153,
//!   the per-org fork-quota bucket capacity.
//! - `STRIPE_WEBHOOK_SIGNING_SECRET` — the `whsec_…` value from your
//!   Stripe webhook endpoint. When set, `POST /v1/stripe/webhook`
//!   accepts events with a valid `Stripe-Signature` header
//!   (HMAC-SHA256 of `timestamp.payload`, within a 300 s replay
//!   window). Unset → the webhook endpoint returns 501
//!   `webhook_disabled`.
//! - `RESEND_API_KEY` + `NANOVM_SIGNUP_FROM` — enable real email
//!   delivery via Resend for the self-serve signup magic link.
//!   `NANOVM_SIGNUP_FROM` must be a verified sender on that Resend
//!   workspace (e.g. `"nanovm <verify@your-domain.com>"`). Unset →
//!   the magic link is logged at `info` (dev/self-hosted only —
//!   NEVER expose that build to real customers).
//! - `NANOVM_SIGNUP_VERIFY_URL` — where the magic-link email points
//!   the recipient. The handler appends `?token=<raw>` at send time.
//!   Typically `https://app.your-saas.com/signup/verify`. Defaults to
//!   `http://localhost:8080/v1/signup/verify` (dev only).
//! - `NANOVM_SIGNUP_TOKEN_TTL_SECS` — magic-link lifetime. Default
//!   `900` (15 min).
//! - `NANOVM_CORS_ORIGIN` — comma-separated list of origins allowed
//!   to hit the API from a browser (e.g.
//!   `https://app.your-saas.com,http://localhost:3000`). Special
//!   value `*` allows any origin (credentials are then dropped —
//!   fine for public read-only surfaces). Unset → no CORS headers
//!   emitted (server-to-server calls unaffected; browsers can't
//!   reach the API). Prereq for the web dashboard.
//! - `NANOVM_BILLING_REPORT_SECS` — enable the metered-billing
//!   reporter and set its tick interval. Unset / `0` → disabled (no
//!   background task, no Stripe traffic). Typical prod: `60`. Only
//!   effective when `--features billing` is on AND
//!   `BillingConfig::from_env()` returned `Some`. Reports
//!   `nanovm_forks_total_by_org` deltas to Stripe `usage_records`
//!   with `action=increment`; the primary subscription item id
//!   (`si_…`) comes from the persisted subscription state that the
//!   webhook handler populated.
//! - `NANOVM_DEFAULT_KERNEL_PATH` — absolute path to the kernel image
//!   used as the fallback when `POST /v1/vms` omits `kernel`. Set to
//!   `/usr/local/share/nanovm/vmlinux` by `Dockerfile.kvm` so
//!   `POST /v1/vms {}` boots against the KVM backend out of the box.
//! - `NANOVM_DEFAULT_ROOTFS_PATH` — same for rootfs. Set to
//!   `/usr/local/share/nanovm/rootfs.ext4` by `Dockerfile.kvm`.
//! - `NANOVM_DEFAULT_KERNEL_CMDLINE` — cmdline used when the request's
//!   cmdline is empty. Recommended default (Firecracker kernel): `console=ttyS0
//!   reboot=k panic=1 pci=off root=/dev/vda rw`. Unset → no cmdline.
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

    // Read `NANOVM_DEFAULT_KERNEL_PATH` / `NANOVM_DEFAULT_ROOTFS_PATH` /
    // `NANOVM_DEFAULT_KERNEL_CMDLINE` once. When set (typically by
    // `Dockerfile.kvm`, which bakes a kernel + rootfs into the image),
    // `POST /v1/vms` with an empty body succeeds against the KVM
    // backend. Unset → the existing "request must supply kernel/rootfs"
    // shape is preserved.
    let vm_defaults = control_plane::VmConfigDefaults::from_env();
    let cmdline_present = vm_defaults.cmdline.is_some();
    match (&vm_defaults.kernel, &vm_defaults.rootfs) {
        (Some(k), Some(r)) => {
            info!(kernel = %k.display(), rootfs = %r.display(), cmdline = cmdline_present, "VmConfig defaults: kernel + rootfs present, empty POST /v1/vms will boot")
        }
        (Some(k), None) => {
            info!(kernel = %k.display(), cmdline = cmdline_present, "VmConfig defaults: kernel-only")
        }
        (None, Some(r)) => {
            info!(rootfs = %r.display(), cmdline = cmdline_present, "VmConfig defaults: rootfs-only")
        }
        (None, None) if cmdline_present => info!(
            "VmConfig defaults: cmdline-only (set NANOVM_DEFAULT_KERNEL_PATH + NANOVM_DEFAULT_ROOTFS_PATH to enable empty-body POST /v1/vms)"
        ),
        (None, None) => info!(
            "VmConfig defaults unset (set NANOVM_DEFAULT_KERNEL_PATH + NANOVM_DEFAULT_ROOTFS_PATH to enable empty-body POST /v1/vms)"
        ),
    }

    let state = AppState::new(hypervisor)
        .with_warm_pool(warm_pool)
        .with_snapshot_store(snapshot_store)
        .with_backend_label(backend_label)
        .with_ownership_map(Arc::new(ownership))
        .with_vm_defaults(vm_defaults)
        .with_marketplace(Arc::new(control_plane::Marketplace::from_env()));
    #[cfg(feature = "billing")]
    let state = state.with_billing(billing_ctx);

    // Metered-billing reporter. Off by default. Enabled when
    // NANOVM_BILLING_REPORT_SECS is set to a positive integer AND
    // BillingCtx is Some. The handle is held for the process lifetime;
    // the graceful-shutdown branch runs first and lets in-flight HTTP
    // finish, then this handle drops, stopping the reporter.
    #[cfg(feature = "billing")]
    let _reporter_handle = spawn_billing_reporter_if_configured(&state);

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

/// Wire the metered-billing reporter if billing is configured AND
/// `NANOVM_BILLING_REPORT_SECS` is set. Returns `None` in every other
/// case (silently — `NANOVM_BILLING_REPORT_SECS` unset is normal for
/// dev / self-hosted).
#[cfg(feature = "billing")]
fn spawn_billing_reporter_if_configured(
    state: &AppState,
) -> Option<control_plane::billing::UsageReporterHandle> {
    let config = control_plane::billing::UsageReporterConfig::from_env()?;
    let ctx = state.billing_ctx_pub()?;
    info!(
        interval_secs = config.interval.as_secs(),
        "metered-billing reporter enabled"
    );
    Some(control_plane::billing::usage_reporter::spawn(
        config,
        Arc::clone(state.metrics()),
        Arc::clone(&ctx.store),
        Arc::clone(&ctx.stripe),
    ))
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
                // In-memory billing store + real Stripe credentials is
                // a live-money footgun: every restart loses the
                // org→customer_id map, so already-live subscriptions
                // become orphans that can't be looked up on the next
                // /v1/billing/portal. Opt-out via
                // `NANOVM_ALLOW_INMEMORY_BILLING=1` for tests / demos.
                let allow_inmemory = std::env::var("NANOVM_ALLOW_INMEMORY_BILLING")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .is_some();
                if !allow_inmemory {
                    tracing::error!(
                        "billing: NANOVM_OWNERSHIP_STORE is required when billing is enabled. \
                         An in-memory store would drop every Stripe customer mapping on restart, \
                         orphaning live subscriptions. Set a SQLite path, or opt into the unsafe \
                         in-memory mode with NANOVM_ALLOW_INMEMORY_BILLING=1 (tests only)."
                    );
                    std::process::exit(1);
                }
                tracing::warn!(
                    "billing: running with InMemoryBillingStore because \
                     NANOVM_ALLOW_INMEMORY_BILLING=1 was set. Restart LOSES all \
                     Stripe customer mappings — DO NOT use with a live Stripe key."
                );
                std::sync::Arc::new(control_plane::billing::InMemoryBillingStore::default())
            }
        };
    let stripe = std::sync::Arc::new(control_plane::billing::StripeClient::new(
        cfg.stripe_secret_key.clone(),
    ));
    let tiers = control_plane::billing::PlanTiers::from_env();
    tracing::info!(
        tier_count = tiers.len(),
        "billing: plan tiers configured (NANOVM_PLAN_TIERS)"
    );
    // Email delivery: Resend if `RESEND_API_KEY` + `NANOVM_SIGNUP_FROM`
    // are both set; otherwise fall through to LogEmailSender (dev /
    // self-hosted). The latter logs magic links at info — DO NOT wire
    // in prod without a real provider.
    let email: std::sync::Arc<dyn control_plane::billing::EmailSender> = match (
        std::env::var("RESEND_API_KEY")
            .ok()
            .filter(|s| !s.is_empty()),
        std::env::var("NANOVM_SIGNUP_FROM")
            .ok()
            .filter(|s| !s.is_empty()),
    ) {
        (Some(key), Some(from)) => {
            tracing::info!(
                from = %from,
                "billing: signup emails via Resend (RESEND_API_KEY + NANOVM_SIGNUP_FROM set)"
            );
            std::sync::Arc::new(control_plane::billing::ResendEmailSender::new(key, from))
        }
        _ => {
            tracing::warn!(
                "billing: signup emails will be logged, not sent \
                 (set RESEND_API_KEY + NANOVM_SIGNUP_FROM to enable delivery)"
            );
            std::sync::Arc::new(control_plane::billing::LogEmailSender)
        }
    };
    Some(control_plane::billing::BillingCtx {
        config: cfg,
        store,
        stripe,
        tiers,
        email,
    })
}
