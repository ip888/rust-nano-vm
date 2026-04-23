//! HTTP handlers and router wiring.
//!
//! The REST surface intentionally mirrors the [`Hypervisor`] trait one-for-one,
//! so the mental model is stable and the control plane stays a thin shell.
//!
//! | Method | Path                                | Trait method   |
//! |--------|-------------------------------------|----------------|
//! | POST   | `/v1/vms`                           | `create_vm`    |
//! | GET    | `/v1/vms`                           | `list_vms`     |
//! | GET    | `/v1/vms/:id`                       | `state`        |
//! | POST   | `/v1/vms/:id/start`                 | `start`        |
//! | POST   | `/v1/vms/:id/stop`                  | `stop`         |
//! | POST   | `/v1/vms/:id/snapshot`              | `snapshot`     |
//! | DELETE | `/v1/vms/:id`                       | `destroy`      |
//! | POST   | `/v1/snapshots/:id/restore`         | `restore`      |
//! | GET    | `/healthz`                          | —              |
//!
//! Hypervisor calls are synchronous and cheap for `vm-mock` so we call them
//! directly from async handlers. Real backends (M1+) should wrap expensive
//! operations in `tokio::task::spawn_blocking`.

use std::sync::Arc;

use axum::{
    extract::{
        rejection::{JsonRejection, PathRejection},
        Path, State,
    },
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use tower_http::trace::TraceLayer;
use vm_core::{Hypervisor, SnapshotId, VmId};

use crate::api::{CreateVmRequest, SnapshotDto, VmHandleDto, VmListResponse, VmStateResponse};
use crate::error::ApiError;

/// Shared state plumbed into every handler.
#[derive(Clone)]
pub struct AppState {
    hypervisor: Arc<dyn Hypervisor>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("hypervisor", &"<dyn Hypervisor>")
            .finish()
    }
}

impl AppState {
    /// Construct a new [`AppState`] wrapping the given hypervisor.
    pub fn new(hypervisor: Arc<dyn Hypervisor>) -> Self {
        Self { hypervisor }
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
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/vms", get(list_vms).post(create_vm))
        .route("/v1/vms/:id", get(get_vm).delete(destroy_vm))
        .route("/v1/vms/:id/start", post(start_vm))
        .route("/v1/vms/:id/stop", post(stop_vm))
        .route("/v1/vms/:id/snapshot", post(snapshot_vm))
        .route("/v1/snapshots/:id/restore", post(restore_snapshot))
        .layer(TraceLayer::new_for_http())
}

async fn healthz() -> &'static str {
    "ok"
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

async fn list_vms(State(state): State<AppState>) -> Result<Json<VmListResponse>, ApiError> {
    let handles = state.hypervisor.list_vms()?;
    Ok(Json(VmListResponse::new(handles)))
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
) -> Result<(StatusCode, Json<SnapshotDto>), ApiError> {
    let Path(id) = id?;
    let snap = state.hypervisor.snapshot(VmId(id))?;
    Ok((StatusCode::CREATED, Json(snap.into())))
}

async fn restore_snapshot(
    State(state): State<AppState>,
    id: Result<Path<u64>, PathRejection>,
) -> Result<(StatusCode, Json<VmHandleDto>), ApiError> {
    let Path(id) = id?;
    let handle = state.hypervisor.restore(SnapshotId(id))?;
    Ok((StatusCode::CREATED, Json(handle.into())))
}
