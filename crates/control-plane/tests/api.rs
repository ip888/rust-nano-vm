//! End-to-end tests for the REST API, driven against a `MockHypervisor`
//! backend via `tower::ServiceExt::oneshot`. No network, no KVM.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Router,
};
use control_plane::{router, AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use vm_mock::MockHypervisor;

fn app() -> Router {
    let hv: Arc<dyn vm_core::Hypervisor> = Arc::new(MockHypervisor::new());
    router(AppState::new(hv))
}

async fn send(app: Router, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&v).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            // Plain-text body (e.g. `/healthz`).
            Value::String(String::from_utf8_lossy(&bytes).into_owned())
        })
    };
    (status, json)
}

#[tokio::test]
async fn healthz_returns_ok() {
    let (status, body) = send(app(), Method::GET, "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::String("ok".into()));
}

#[tokio::test]
async fn create_vm_with_defaults_returns_created_handle() {
    let (status, body) = send(app(), Method::POST, "/v1/vms", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(body["id"].is_u64());
    assert_eq!(body["state"], "created");
    assert!(body["display"].as_str().unwrap().starts_with("vm-"));
}

#[tokio::test]
async fn create_vm_accepts_explicit_config() {
    let (status, body) = send(
        app(),
        Method::POST,
        "/v1/vms",
        Some(json!({
            "vcpus": 4,
            "memory_mib": 1024,
            "cmdline": "console=ttyS0",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["state"], "created");
}

#[tokio::test]
async fn start_stop_roundtrip_through_state_endpoint() {
    let app = app();

    let (_, handle) = send(app.clone(), Method::POST, "/v1/vms", Some(json!({}))).await;
    let id = handle["id"].as_u64().unwrap();

    let (status, _) = send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/start"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = send(app.clone(), Method::GET, &format!("/v1/vms/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "running");

    let (status, _) = send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/stop"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = send(app.clone(), Method::GET, &format!("/v1/vms/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "stopped");
}

#[tokio::test]
async fn snapshot_then_restore_returns_new_handle() {
    let app = app();

    let (_, handle) = send(app.clone(), Method::POST, "/v1/vms", Some(json!({}))).await;
    let vm_id = handle["id"].as_u64().unwrap();

    let (status, snap) = send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{vm_id}/snapshot"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let snap_id = snap["id"].as_u64().unwrap();
    assert!(snap["display"].as_str().unwrap().starts_with("snap-"));

    let (status, restored) = send(
        app.clone(),
        Method::POST,
        &format!("/v1/snapshots/{snap_id}/restore"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let restored_id = restored["id"].as_u64().unwrap();
    assert_ne!(restored_id, vm_id, "restore must return a fresh VM id");
}

#[tokio::test]
async fn destroy_then_get_is_not_found() {
    let app = app();

    let (_, handle) = send(app.clone(), Method::POST, "/v1/vms", Some(json!({}))).await;
    let id = handle["id"].as_u64().unwrap();

    let (status, _) = send(app.clone(), Method::DELETE, &format!("/v1/vms/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = send(app.clone(), Method::GET, &format!("/v1/vms/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "unknown_vm");
}

#[tokio::test]
async fn get_unknown_vm_returns_structured_error() {
    let (status, body) = send(app(), Method::GET, "/v1/vms/99999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "unknown_vm");
    assert!(body["error"]["message"].as_str().unwrap().contains("vm-"));
}

#[tokio::test]
async fn double_start_returns_invalid_transition_conflict() {
    let app = app();

    let (_, handle) = send(app.clone(), Method::POST, "/v1/vms", Some(json!({}))).await;
    let id = handle["id"].as_u64().unwrap();

    let (status, _) = send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/start"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/start"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "invalid_transition");
}

#[tokio::test]
async fn restore_unknown_snapshot_is_not_found() {
    let (status, body) = send(app(), Method::POST, "/v1/snapshots/99999/restore", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "unknown_snapshot");
}
