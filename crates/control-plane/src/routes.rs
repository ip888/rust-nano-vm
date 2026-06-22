//! HTTP handlers and router wiring.
//!
//! The REST surface intentionally mirrors the [`Hypervisor`] trait one-for-one,
//! so the mental model is stable and the control plane stays a thin shell.
//!
//! | Method | Path                                | Trait method      |
//! |--------|-------------------------------------|-------------------|
//! | POST   | `/v1/vms`                           | `create_vm`       |
//! | GET    | `/v1/vms`                           | `list_vms`        |
//! | GET    | `/v1/vms/:id`                       | `state`           |
//! | POST   | `/v1/vms/:id/start`                 | `start`           |
//! | POST   | `/v1/vms/:id/stop`                  | `stop`            |
//! | POST   | `/v1/vms/:id/snapshot`              | `snapshot`        |
//! | DELETE | `/v1/vms/:id`                       | `destroy`         |
//! | GET    | `/v1/snapshots`                     | `list_snapshots`  |
//! | DELETE | `/v1/snapshots/:id`                 | `delete_snapshot` |
//! | POST   | `/v1/snapshots/:id/restore`         | `restore`         |
//! | GET    | `/healthz`                          | —                 |
//!
//! Hypervisor calls are synchronous and cheap for `vm-mock` so we call them
//! directly from async handlers. Real backends (M1+) should wrap expensive
//! operations in `tokio::task::spawn_blocking`.

use std::sync::Arc;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use axum::{
    body::Bytes,
    extract::{
        rejection::{JsonRejection, PathRejection},
        Path, Query, State,
    },
    http::{HeaderMap, StatusCode},
    middleware,
    routing::{get, post},
    Json, Router,
};
use tower_http::trace::TraceLayer;
use vm_core::{Hypervisor, SnapshotId, VmId};

use crate::api::{
    openapi_spec, CreateVmRequest, ExecRequest, ExecResponse, FilePathQuery, FileReadResponse,
    FileWriteRequest, FileWrittenResponse, ForkResponseDto, HealthResponse, ListQuery, SnapshotDto,
    SnapshotListEntry, SnapshotListResponse, SnapshotRequest, UsageResponseDto, VmHandleDto,
    VmListEntry, VmListResponse, VmStateResponse,
};
use crate::audit;
use crate::auth;
use crate::error::ApiError;
use crate::request_id;
use crate::time::rfc3339_now;

/// Per-token fork usage — the basis for usage-based billing on the fork API.
#[derive(Debug, Default, Clone, Copy)]
pub struct ForkUsage {
    /// Number of successful forks this token has performed.
    pub count: u64,
    /// Total wall-time (ms) spent restoring those forks.
    pub total_ms: u64,
}

/// Shared state plumbed into every handler.
#[derive(Clone)]
pub struct AppState {
    hypervisor: Arc<dyn Hypervisor>,
    /// Per-token fork counters keyed by bearer-token. Locked briefly per call.
    fork_usage: Arc<Mutex<HashMap<String, ForkUsage>>>,
    /// Per-token fork rate limiter. Shared `Arc` so handlers can poll
    /// without contending on `AppState`'s clone.
    fork_quota: Arc<crate::ForkQuota>,
    /// Prometheus counters exposed at `/metrics`.
    metrics: Arc<crate::Metrics>,
    /// Process start time (monotonic clock) — used to compute uptime
    /// reported on `GET /v1/health`. Set once when the first `AppState`
    /// is constructed and shared across clones.
    start_instant: Instant,
    /// Wall-clock boot time as a RFC 3339 string — surfaced on
    /// `GET /v1/health`. Frozen at AppState construction so subsequent
    /// system clock changes don't drift the reported boot time.
    started_at: String,
    /// Human-readable backend label surfaced on `GET /v1/health`
    /// (e.g. `"mock"`, `"kvm"`). Defaults to `"mock"` in `new()`.
    backend_label: &'static str,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("hypervisor", &"<dyn Hypervisor>")
            .field("fork_usage", &"<Arc<Mutex<HashMap>>>")
            .finish()
    }
}

impl AppState {
    /// Construct a new [`AppState`] wrapping the given hypervisor. The
    /// fork quota defaults to the values from
    /// [`ForkQuota::from_env`](crate::ForkQuota::from_env); use
    /// [`with_fork_quota`](Self::with_fork_quota) for an explicit override.
    /// Backend label defaults to `"mock"`; real backends should call
    /// [`with_backend_label`](Self::with_backend_label).
    pub fn new(hypervisor: Arc<dyn Hypervisor>) -> Self {
        Self::with_fork_quota(hypervisor, Arc::new(crate::ForkQuota::from_env()))
    }

    /// Construct an [`AppState`] with an explicit [`ForkQuota`]. Useful
    /// in tests that want a tight bucket without env juggling.
    pub fn with_fork_quota(
        hypervisor: Arc<dyn Hypervisor>,
        fork_quota: Arc<crate::ForkQuota>,
    ) -> Self {
        Self {
            hypervisor,
            fork_usage: Arc::new(Mutex::new(HashMap::new())),
            fork_quota,
            metrics: Arc::new(crate::Metrics::new()),
            start_instant: Instant::now(),
            started_at: rfc3339_now(),
            backend_label: "mock",
        }
    }

    /// Override the backend label surfaced on `GET /v1/health`. Real
    /// backends (e.g. the KVM binary) call this with `"kvm"` so
    /// monitoring can tell at a glance which backend a process is
    /// running.
    pub fn with_backend_label(mut self, label: &'static str) -> Self {
        self.backend_label = label;
        self
    }

    /// Borrow the metrics collector. Test-only API for asserting on
    /// recorded counters without going through `/metrics`.
    #[doc(hidden)]
    pub fn metrics(&self) -> &Arc<crate::Metrics> {
        &self.metrics
    }
}

/// Build the REST router. The returned [`Router`] is parameterised over
/// [`AppState`]; callers bind a concrete state with `.with_state(...)`
/// before serving. Keeping state late-bound lets callers layer middleware
/// or compose this router into a larger app without constructing the
/// hypervisor backend first.
///
/// ```no_run
/// # use std::sync::Arc;
/// # use axum::Router;
/// # use control_plane::{router, AppState};
/// # use vm_mock::MockHypervisor;
/// let state = AppState::new(Arc::new(MockHypervisor::new()));
/// let app: Router = router().with_state(state);
/// # let _ = app;
/// ```
pub fn router() -> Router<AppState> {
    // `/v1/*` is guarded by [`auth::require_token`]. `/healthz` is
    // intentionally exempt so external liveness probes don't carry secrets.
    // The middleware reads `Arc<ApiTokens>` from request extensions; callers
    // install it via `.layer(Extension(Arc::new(tokens)))` before serving.
    let v1 = Router::new()
        .route("/vms", get(list_vms).post(create_vm))
        .route("/vms/:id", get(get_vm).delete(destroy_vm))
        .route("/vms/:id/start", post(start_vm))
        .route("/vms/:id/stop", post(stop_vm))
        .route("/vms/:id/snapshot", post(snapshot_vm))
        .route("/vms/:id/exec", post(exec_vm))
        .route("/vms/:id/files", get(read_file).post(write_file))
        .route("/snapshots", get(list_snapshots))
        .route("/snapshots/:id", axum::routing::delete(delete_snapshot))
        .route("/snapshots/:id/restore", post(restore_snapshot))
        // `/fork` is the metered, customer-facing form of `/restore`: same op
        // under the hood (CoW fork from the snapshot), plus per-token usage
        // accounting so we can bill on fork count + latency.
        .route("/snapshots/:id/fork", post(fork_snapshot))
        .route("/usage", get(usage))
        .route("/health", get(health))
        // route_layer is bottom-up: the LAST `.route_layer` runs FIRST.
        // require_audit must see *authenticated* calls only, so register
        // it INNER (here) and require_token OUTER (after this).
        .route_layer(middleware::from_fn(audit::require_audit))
        .route_layer(middleware::from_fn(auth::require_token));

    Router::new()
        .route("/healthz", get(healthz))
        .route("/openapi.json", get(openapi))
        // /metrics is on the outer router (no auth). Operators that don't
        // want it publicly reachable should bind to 127.0.0.1 or restrict
        // at the reverse proxy — same convention as Prometheus exporters.
        .route("/metrics", get(metrics_text))
        .nest("/v1", v1)
        // OUTERMOST so /healthz, /metrics, /openapi.json, /v1/* all get a
        // request id. The audit appender reads RequestId out of
        // extensions; tracing logs inherit the id via the span.
        .layer(middleware::from_fn(request_id::with_request_id))
        .layer(TraceLayer::new_for_http())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn openapi() -> Json<serde_json::Value> {
    Json(openapi_spec())
}

/// `GET /metrics` — Prometheus text exposition. Unauthenticated by design
/// so scrapers don't carry secrets.
async fn metrics_text(
    State(state): State<AppState>,
) -> ([(&'static str, &'static str); 1], String) {
    (
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        state.metrics.render_text(),
    )
}

// Each handler takes extractors as `Result<Extractor, Rejection>` and `?`s the
// rejection into `ApiError`. This keeps extractor failures (malformed JSON,
// non-numeric path segment, wrong content-type) in the same structured error
// envelope as hypervisor errors, instead of leaking axum's plain-text defaults.

async fn create_vm(
    State(state): State<AppState>,
    body: Result<Json<CreateVmRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<VmHandleDto>), ApiError> {
    let Json(req) = body?;
    let handle = state.hypervisor.create_vm(&req.into())?;
    Ok((StatusCode::CREATED, Json(handle.into())))
}

async fn list_vms(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<VmListResponse>, ApiError> {
    let limit = query.validated_limit()?;
    let after = query.after.unwrap_or(0);

    // Fetch all ids first, sort by id, drop everything <= after, then
    // enrich only the rows we keep. Enriching the dropped rows would
    // turn a 100-row page over a 100k-VM hypervisor into 100k vm_meta
    // calls — which would defeat the point of pagination.
    let mut handles = state.hypervisor.list_vms()?;
    handles.sort_by_key(|h| h.id.0);
    let mut filtered: Vec<_> = handles.into_iter().filter(|h| h.id.0 > after).collect();
    let has_more = filtered.len() > limit as usize;
    filtered.truncate(limit as usize);

    let mut vms = Vec::with_capacity(filtered.len());
    for handle in filtered {
        // Best-effort metadata enrichment — same degrade-gracefully
        // pattern as list_snapshots: Unsupported (backend can't
        // surface geometry) or UnknownVm (raced with destroy) → id-
        // only row. Other backend errors propagate as 5xx.
        let entry = match state.hypervisor.vm_meta(handle.id) {
            Ok(meta) => VmListEntry::from_meta(meta),
            Err(vm_core::VmError::Unsupported(_)) | Err(vm_core::VmError::UnknownVm(_)) => {
                VmListEntry::id_only(handle)
            }
            Err(e) => return Err(ApiError::from(e)),
        };
        vms.push(entry);
    }

    let next = has_more.then(|| vms.last().map(|e| e.id)).flatten();
    Ok(Json(VmListResponse { vms, next }))
}

async fn get_vm(
    State(state): State<AppState>,
    id: Result<Path<u64>, PathRejection>,
) -> Result<Json<VmStateResponse>, ApiError> {
    let Path(id) = id?;
    let vm_id = VmId(id);
    let vm_state = state.hypervisor.state(vm_id)?;
    Ok(Json(VmStateResponse::new(vm_id, vm_state)))
}

async fn start_vm(
    State(state): State<AppState>,
    id: Result<Path<u64>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Path(id) = id?;
    state.hypervisor.start(VmId(id))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn stop_vm(
    State(state): State<AppState>,
    id: Result<Path<u64>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Path(id) = id?;
    state.hypervisor.stop(VmId(id))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn destroy_vm(
    State(state): State<AppState>,
    id: Result<Path<u64>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Path(id) = id?;
    state.hypervisor.destroy(VmId(id))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn snapshot_vm(
    State(state): State<AppState>,
    id: Result<Path<u64>, PathRejection>,
    body: Bytes,
) -> Result<(StatusCode, Json<SnapshotDto>), ApiError> {
    let Path(id) = id?;
    // Body is optional. Empty (legacy callers) → default request.
    // Otherwise must parse as SnapshotRequest JSON, mirroring the
    // BadJson envelope shape used by the rest of the API.
    let req = if body.is_empty() {
        SnapshotRequest::default()
    } else {
        serde_json::from_slice::<SnapshotRequest>(&body)
            .map_err(|e| ApiError::Bad(format!("snapshot body: {e}")))?
    };
    let snap = state.hypervisor.snapshot(VmId(id))?;
    let mut dto: SnapshotDto = snap.into();
    if let Some(dir) = req.to_dir {
        // Pull the captured geometry from the backend, render it as a
        // snapshot::Manifest, and persist alongside the directory.
        // Errors here surface as Backend(...) — same envelope shape as
        // any other backend failure.
        let meta = state.hypervisor.snapshot_meta(snap)?;
        let mut manifest = snapshot::Manifest::new(
            meta.id.0,
            meta.memory_bytes,
            meta.page_size,
            meta.vcpu_count,
        );
        manifest.kernel_cmdline = meta.kernel_cmdline;
        manifest
            .write_to_dir(&dir)
            .map_err(|e| vm_core::VmError::Backend(format!("snapshot persist: {e}")))?;
        dto.dir = Some(dir);
    }
    Ok((StatusCode::CREATED, Json(dto)))
}

async fn restore_snapshot(
    State(state): State<AppState>,
    id: Result<Path<u64>, PathRejection>,
) -> Result<(StatusCode, Json<VmHandleDto>), ApiError> {
    let Path(id) = id?;
    let handle = state.hypervisor.restore(SnapshotId(id))?;
    Ok((StatusCode::CREATED, Json(handle.into())))
}

/// `POST /v1/snapshots/:id/fork` — the metered customer-facing form of
/// restore. Same operation under the hood, but the response carries the
/// per-fork latency (the headline product number) and per-token usage is
/// accumulated for billing. Auth-off mode (no bearer) still serves the
/// fork; only the usage counter is skipped.
async fn fork_snapshot(
    State(state): State<AppState>,
    id: Result<Path<u64>, PathRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<ForkResponseDto>), ApiError> {
    let Path(id) = id?;
    let bearer = extract_bearer(&headers);
    // Gate before doing any work — quota exhaustion is the common bad-day
    // signal, so it should be cheap to surface.
    if let Err(retry_after_secs) = state.fork_quota.try_acquire(bearer.as_deref()) {
        let fp = bearer
            .as_deref()
            .map(token_fingerprint)
            .unwrap_or_else(|| "anonymous".to_owned());
        state.metrics.record_throttled(&fp);
        return Err(ApiError::TooManyRequests {
            code: "fork_quota_exceeded",
            message: format!("fork quota exceeded; retry after {retry_after_secs} second(s)"),
            retry_after_secs,
        });
    }
    let started = Instant::now();
    let handle = state.hypervisor.restore(SnapshotId(id))?;
    let fork_ms = started.elapsed().as_millis() as u64;

    let mut fork_count = 1u64;
    let mut fork_total_ms = fork_ms;
    let fp = bearer
        .as_deref()
        .map(token_fingerprint)
        .unwrap_or_else(|| "anonymous".to_owned());
    state.metrics.record_fork(&fp, fork_ms);
    if let Some(token) = bearer {
        let mut usage = state
            .fork_usage
            .lock()
            .map_err(|_| ApiError::Internal("fork_usage mutex poisoned"))?;
        let entry = usage.entry(token).or_default();
        entry.count = entry.count.saturating_add(1);
        entry.total_ms = entry.total_ms.saturating_add(fork_ms);
        fork_count = entry.count;
        fork_total_ms = entry.total_ms;
    }

    Ok((
        StatusCode::CREATED,
        Json(ForkResponseDto {
            vm: handle.into(),
            fork_ms,
            fork_count,
            fork_total_ms,
        }),
    ))
}

/// `GET /v1/usage` — the caller's fork-usage counters. The token is
/// reported as a non-cryptographic fingerprint so the response is safe to
/// log / show to the caller, never the raw bearer.
async fn usage(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<UsageResponseDto>, ApiError> {
    let token = extract_bearer(&headers)
        .ok_or_else(|| ApiError::Unauthorized("missing bearer for usage query".into()))?;
    let entry = state
        .fork_usage
        .lock()
        .map_err(|_| ApiError::Internal("fork_usage mutex poisoned"))?
        .get(&token)
        .copied()
        .unwrap_or_default();
    Ok(Json(UsageResponseDto {
        token: token_fingerprint(&token),
        fork_count: entry.count,
        fork_total_ms: entry.total_ms,
    }))
}

/// `GET /v1/health` — structured health endpoint with backend label,
/// crate version, uptime, and the wall-clock boot time. Auth-gated
/// (lives under `/v1/*`) so the version disclosure isn't a free
/// fingerprinting surface; lightweight liveness probes should keep using
/// `/healthz`.
async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        backend: state.backend_label,
        version: env!("CARGO_PKG_VERSION"),
        uptime_secs: state.start_instant.elapsed().as_secs(),
        started_at: state.started_at.clone(),
    })
}

/// Pull the bearer token out of the `Authorization: Bearer …` header, if
/// present. Returns `None` for missing / malformed headers (auth-off mode
/// or unauthenticated probes).
fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(|s| s.to_owned())
}

/// Non-cryptographic fingerprint of a token: `tok-<first4>-<len>`. Lets
/// operators correlate audit/usage entries without ever logging the raw
/// secret. Same shape the audit log uses.
pub(crate) fn token_fingerprint(token: &str) -> String {
    let head: String = token.chars().take(4).collect();
    format!("tok-{head}-{}", token.len())
}

async fn list_snapshots(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<SnapshotListResponse>, ApiError> {
    let limit = query.validated_limit()?;
    let after = query.after.unwrap_or(0);

    // Same filter-then-enrich shape as `list_vms`: snapshot_meta() can
    // be expensive (file-system reads for the KVM backend), so only
    // call it for rows we actually return.
    let mut ids = state.hypervisor.list_snapshots()?;
    ids.sort_by_key(|s| s.0);
    let mut filtered: Vec<_> = ids.into_iter().filter(|s| s.0 > after).collect();
    let has_more = filtered.len() > limit as usize;
    filtered.truncate(limit as usize);

    let mut snapshots = Vec::with_capacity(filtered.len());
    for id in filtered {
        // Best-effort metadata enrichment. Two failure modes are
        // expected and we degrade gracefully:
        // - Unsupported: the backend can't surface geometry. Keep the
        //   entry with id + display so the listing never silently
        //   swallows snapshots that exist.
        // - UnknownSnapshot: a concurrent delete raced with our list.
        //   Same handling — id-only row.
        // Any other error means the backend is unhealthy; bubble it up
        // as a 5xx so the caller learns rather than getting a partial
        // list with no signal.
        let entry = match state.hypervisor.snapshot_meta(id) {
            Ok(meta) => SnapshotListEntry::from_meta(meta),
            Err(vm_core::VmError::Unsupported(_)) | Err(vm_core::VmError::UnknownSnapshot(_)) => {
                SnapshotListEntry::id_only(id)
            }
            Err(e) => return Err(ApiError::from(e)),
        };
        snapshots.push(entry);
    }

    let next = has_more.then(|| snapshots.last().map(|e| e.id)).flatten();
    Ok(Json(SnapshotListResponse { snapshots, next }))
}

async fn delete_snapshot(
    State(state): State<AppState>,
    id: Result<Path<u64>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Path(id) = id?;
    state.hypervisor.delete_snapshot(SnapshotId(id))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn exec_vm(
    State(state): State<AppState>,
    id: Result<Path<u64>, PathRejection>,
    body: Result<Json<ExecRequest>, JsonRejection>,
) -> Result<Json<ExecResponse>, ApiError> {
    let Path(id) = id?;
    let Json(req) = body?;
    let result = state.hypervisor.exec_in_guest(VmId(id), req.into())?;
    Ok(Json(result.into()))
}

async fn write_file(
    State(state): State<AppState>,
    id: Result<Path<u64>, PathRejection>,
    body: Result<Json<FileWriteRequest>, JsonRejection>,
) -> Result<Json<FileWrittenResponse>, ApiError> {
    let Path(id) = id?;
    let Json(req) = body?;
    let bytes = state
        .hypervisor
        .write_file(VmId(id), req.path, req.content, req.mode)?;
    Ok(Json(FileWrittenResponse { bytes }))
}

async fn read_file(
    State(state): State<AppState>,
    id: Result<Path<u64>, PathRejection>,
    query: Result<Query<FilePathQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<FileReadResponse>, ApiError> {
    let Path(id) = id?;
    let Query(q) = query.map_err(|e| ApiError::Bad(e.to_string()))?;
    let content = state.hypervisor.read_file(VmId(id), q.path)?;
    Ok(Json(FileReadResponse { content }))
}
