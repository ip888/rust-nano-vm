//! `POST /v1/snapshots/:id/export` and `POST /v1/snapshots/import`.
//!
//! These are the customer-facing endpoints that turn a local
//! snapshot into a durable artifact in S3 / MinIO / etc., and pull
//! one back into a fresh control-plane process. They're the wire
//! face of the "snapshot once → durable → fork anywhere" story.

use axum::{
    extract::{
        rejection::{JsonRejection, PathRejection},
        Path, State,
    },
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use vm_core::SnapshotId;

use crate::error::ApiError;
use crate::routes::AppState;

/// Request body for `POST /v1/snapshots/:id/export`. Empty body
/// (`{}`) is fine — the response carries the URI the snapshot
/// ended up at, derived from the configured store.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ExportRequest {
    /// Optional snapshot id to use as the durable-storage key. When
    /// `None`, the local `:id` is reused. Useful when callers want
    /// a stable, predictable id in the store (e.g. `42` instead of
    /// whatever in-process counter the producer assigned).
    #[serde(default)]
    pub as_snapshot_id: Option<u64>,
}

/// Response body for `POST /v1/snapshots/:id/export`.
#[derive(Debug, Serialize)]
pub(crate) struct ExportResponse {
    /// Display URI of the durable artifact
    /// (`s3://bucket/prefix/snap-42`, `file:///path/snap-42`).
    pub uri: String,
    /// The id under which the snapshot was stored. Pass this back
    /// to the import endpoint to pull it again.
    pub snapshot_id: u64,
}

/// Request body for `POST /v1/snapshots/import`.
#[derive(Debug, Deserialize)]
pub(crate) struct ImportRequest {
    /// Snapshot id to fetch from the durable store. Must have been
    /// previously written (by this control plane or any other; the
    /// on-disk layout is portable).
    pub snapshot_id: u64,
}

/// Response body for `POST /v1/snapshots/import`. The newly-adopted
/// snapshot id is local-process-unique and the customer should use
/// it for subsequent `/restore` or `/fork` calls.
#[derive(Debug, Serialize)]
pub(crate) struct ImportResponse {
    /// Local snapshot id assigned to the adopted snapshot. Use this
    /// in subsequent `/v1/snapshots/:id/restore` or
    /// `/v1/snapshots/:id/fork` calls.
    pub snapshot_id: u64,
    /// Display URI of the source artifact, for log clarity.
    pub source_uri: String,
}

pub(crate) async fn export_snapshot(
    State(state): State<AppState>,
    id: Result<Path<u64>, PathRejection>,
    body: Result<Json<ExportRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ExportResponse>), ApiError> {
    let Path(id) = id?;
    let Json(req) = body.unwrap_or(Json(ExportRequest::default()));
    let store = state
        .snapshot_store()
        .ok_or_else(|| ApiError::Unsupported {
            code: "storage_unsupported",
            message: "no durable snapshot store configured \
                      (set NANOVM_SNAPSHOT_STORE before starting the control plane)"
                .into(),
        })?
        .clone();

    let snap = SnapshotId(id);
    let dir = state
        .hypervisor()
        .snapshot_export_dir(snap)?
        .ok_or_else(|| ApiError::Unsupported {
            code: "snapshot_export_unsupported",
            message: format!("snapshot id {id} has no on-disk representation on this backend"),
        })?;

    // Move the blocking store call onto the blocking pool so we
    // don't park the reactor for the duration of a multi-MiB
    // upload.
    let target_id = req.as_snapshot_id.unwrap_or(id);
    let store_uri = store.display();
    let display_uri = format!("{store_uri}/snap-{target_id}");
    let store_for_task = store.clone();
    tokio::task::spawn_blocking(move || store_for_task.put(target_id, &dir))
        .await
        .map_err(|e| ApiError::InternalDyn(format!("export task panicked: {e}")))?
        .map_err(|e| store_err_to_api(e, &display_uri))?;

    Ok((
        StatusCode::OK,
        Json(ExportResponse {
            uri: display_uri,
            snapshot_id: target_id,
        }),
    ))
}

pub(crate) async fn import_snapshot(
    State(state): State<AppState>,
    body: Result<Json<ImportRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ImportResponse>), ApiError> {
    let Json(req) = body?;
    let store = state
        .snapshot_store()
        .ok_or_else(|| ApiError::Unsupported {
            code: "storage_unsupported",
            message: "no durable snapshot store configured \
                      (set NANOVM_SNAPSHOT_STORE before starting the control plane)"
                .into(),
        })?
        .clone();

    let store_uri = store.display();
    let display_uri = format!("{store_uri}/snap-{}", req.snapshot_id);

    // Download into a per-request temp directory. We never reuse the
    // path across calls so concurrent imports of the same store id
    // don't race on the disk layout.
    let pid = std::process::id();
    let nonce = uniq_nonce();
    let target_dir = std::env::temp_dir().join(format!(
        "nanovm-snapshot-import-{}-{}-{}",
        pid, req.snapshot_id, nonce
    ));
    let store_for_task = store.clone();
    let target_for_task = target_dir.clone();
    let src_snap_id = req.snapshot_id;
    tokio::task::spawn_blocking(move || store_for_task.get(src_snap_id, &target_for_task))
        .await
        .map_err(|e| ApiError::InternalDyn(format!("import task panicked: {e}")))?
        .map_err(|e| store_err_to_api(e, &display_uri))?;

    let hv = state.hypervisor().clone();
    let target_dir_for_adopt = target_dir.clone();
    let local_id = tokio::task::spawn_blocking(move || hv.snapshot_adopt(&target_dir_for_adopt))
        .await
        .map_err(|e| ApiError::InternalDyn(format!("adopt task panicked: {e}")))?
        .map_err(ApiError::from)?;

    // Best-effort cleanup of the temp dir — the backend has copied
    // what it needs into its own storage.
    let _ = std::fs::remove_dir_all(&target_dir);

    Ok((
        StatusCode::CREATED,
        Json(ImportResponse {
            snapshot_id: local_id.0,
            source_uri: display_uri,
        }),
    ))
}

/// Map a `snapshot::StoreError` into the right `ApiError` so
/// customers see meaningful HTTP status codes rather than generic 5xx.
fn store_err_to_api(err: snapshot::StoreError, uri: &str) -> ApiError {
    use snapshot::StoreError;
    match err {
        StoreError::NotFound(id) => ApiError::NotFound {
            code: "snapshot_not_in_store",
            message: format!("{uri}: snapshot id {id} not found"),
        },
        StoreError::Auth(msg) => ApiError::Unauthorized(format!("snapshot store auth: {msg}")),
        StoreError::BadUri(msg) => ApiError::Bad(format!("snapshot store uri: {msg}")),
        StoreError::Format(e) => ApiError::Bad(format!("snapshot store format: {e}")),
        // Network and IO failures bubble up as 5xx — they're
        // operational rather than client-fixable.
        StoreError::Network(msg) => ApiError::InternalDyn(format!("snapshot store network: {msg}")),
        StoreError::Io(e) => ApiError::InternalDyn(format!("snapshot store io: {e}")),
        // `StoreError` is #[non_exhaustive]; fall back to a generic
        // 500 for any future variants. Re-route them explicitly if a
        // better classification matters.
        other => ApiError::InternalDyn(format!("snapshot store: {other}")),
    }
}

/// Cheap process-local id source for the import temp-dir name.
/// Doesn't need to be cryptographically unique — collisions only
/// matter within a single process, and any in-flight import already
/// holds its own temp dir for the duration.
fn uniq_nonce() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}
