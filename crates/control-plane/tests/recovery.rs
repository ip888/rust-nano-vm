use std::sync::Arc;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Extension, Router,
};
use control_plane::{router, ApiTokens, AppState, RecoveryStore};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use vm_core::Hypervisor;
use vm_mock::MockHypervisor;

async fn send(app: Router, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    let body = if let Some(body) = body {
        request = request.header("content-type", "application/json");
        Body::from(serde_json::to_vec(&body).unwrap())
    } else {
        Body::empty()
    };
    let response = app.oneshot(request.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, value)
}

fn app(state: AppState) -> Router {
    router()
        .layer(Extension(Arc::new(ApiTokens::default())))
        .with_state(state)
}

#[tokio::test]
async fn control_plane_restart_recreates_vms_snapshots_and_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let state_path = temp.path().join("control-plane-state.json");
    let snapshot_path = temp.path().join("snapshot-store");
    let snapshot_store: Arc<dyn snapshot::SnapshotStore> =
        Arc::new(snapshot::FsSnapshotStore::new(&snapshot_path));

    let first_hypervisor: Arc<dyn Hypervisor> = Arc::new(MockHypervisor::new());
    let first_store = Arc::new(RecoveryStore::open(&state_path).unwrap());
    let first = app(AppState::new(first_hypervisor)
        .with_snapshot_store(Some(snapshot_store.clone()))
        .with_recovery_store(first_store));

    let (status, vm) = send(
        first.clone(),
        Method::POST,
        "/v1/vms",
        Some(json!({
            "memory_mib": 256,
            "port_forwards": [{
                "guest_port": 8080,
                "host_address": "127.0.0.1",
                "host_port": 43123
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {vm}");
    let vm_id = vm["id"].as_u64().unwrap();
    let (status, body) = send(
        first.clone(),
        Method::POST,
        &format!("/v1/vms/{vm_id}/start"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "body: {body}");

    let (status, snapshot) = send(
        first.clone(),
        Method::POST,
        &format!("/v1/vms/{vm_id}/snapshot"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {snapshot}");
    let snapshot_id = snapshot["id"].as_u64().unwrap();
    let (status, exported) = send(
        first.clone(),
        Method::POST,
        &format!("/v1/snapshots/{snapshot_id}/export"),
        Some(json!({"as_snapshot_id": 7001})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {exported}");
    let (status, conflict) = send(
        first.clone(),
        Method::POST,
        &format!("/v1/snapshots/{snapshot_id}/export"),
        Some(json!({"as_snapshot_id": 7001})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "immutable store keys must not be overlaid: {conflict}"
    );
    let (status, restored) = send(
        first,
        Method::POST,
        &format!("/v1/snapshots/{snapshot_id}/restore"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {restored}");

    let restarted_hypervisor: Arc<dyn Hypervisor> = Arc::new(MockHypervisor::new());
    let restarted_store = Arc::new(RecoveryStore::open(&state_path).unwrap());
    let restarted_state = AppState::new(restarted_hypervisor)
        .with_snapshot_store(Some(snapshot_store))
        .with_recovery_store(restarted_store);
    let report = restarted_state.reconcile_restart().unwrap();
    assert_eq!(report.recreated(), 3, "report: {report:?}");
    assert_eq!(report.expired(), 0);
    assert_eq!(report.unrecoverable(), 0);

    let restarted = app(restarted_state);
    let (status, vms) = send(restarted.clone(), Method::GET, "/v1/vms", None).await;
    assert_eq!(status, StatusCode::OK, "body: {vms}");
    assert_eq!(vms["vms"].as_array().unwrap().len(), 2);
    assert!(vms["vms"]
        .as_array()
        .unwrap()
        .iter()
        .all(|vm| vm["state"] == "running"));

    let (status, snapshots) = send(restarted, Method::GET, "/v1/snapshots", None).await;
    assert_eq!(status, StatusCode::OK, "body: {snapshots}");
    assert_eq!(snapshots["snapshots"].as_array().unwrap().len(), 1);
}
