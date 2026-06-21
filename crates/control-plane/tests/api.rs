//! End-to-end tests for the REST API, driven against a `MockHypervisor`
//! backend via `tower::ServiceExt::oneshot`. No network, no KVM.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Extension, Router,
};
use control_plane::{router, ApiTokens, AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use vm_mock::MockHypervisor;

/// Router with auth disabled (empty token set). Most tests use this so they
/// don't need to juggle headers.
fn app() -> Router {
    app_with_tokens(ApiTokens::default())
}

/// Router configured with an explicit token set. Use for auth tests.
fn app_with_tokens(tokens: ApiTokens) -> Router {
    let hv: Arc<dyn vm_core::Hypervisor> = Arc::new(MockHypervisor::new());
    router()
        .layer(Extension(Arc::new(tokens)))
        .with_state(AppState::new(hv))
}

async fn send(app: Router, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    send_with(app, method, uri, body, None).await
}

async fn send_with(
    app: Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(tok) = bearer {
        builder = builder.header("authorization", format!("Bearer {tok}"));
    }
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
async fn openapi_json_returns_document() {
    let (status, body) = send(app(), Method::GET, "/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["openapi"], "3.1.0");
    assert_eq!(body["info"]["title"], "rust-nano-vm control-plane API");
    assert!(body["paths"]["/v1/vms"].is_object());
    assert!(body["paths"]["/v1/snapshots/{id}/restore"].is_object());
    assert!(body["paths"]["/openapi.json"].is_object());
    assert!(body["components"]["schemas"]["VmHandleDto"].is_object());
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

// --- Listing -----

#[tokio::test]
async fn list_vms_is_empty_initially() {
    let (status, body) = send(app(), Method::GET, "/v1/vms", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["vms"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn list_vms_returns_all_created_vms_with_state() {
    let app = app();
    let mut ids = Vec::new();
    for _ in 0..3 {
        let (_, h) = send(app.clone(), Method::POST, "/v1/vms", Some(json!({}))).await;
        ids.push(h["id"].as_u64().unwrap());
    }
    // Start the second, start+stop the third — first stays Created.
    send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{}/start", ids[1]),
        None,
    )
    .await;
    send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{}/start", ids[2]),
        None,
    )
    .await;
    send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{}/stop", ids[2]),
        None,
    )
    .await;

    let (status, body) = send(app.clone(), Method::GET, "/v1/vms", None).await;
    assert_eq!(status, StatusCode::OK);
    let vms = body["vms"].as_array().unwrap();
    assert_eq!(vms.len(), 3);

    let mut by_id: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    for vm in vms {
        by_id.insert(
            vm["id"].as_u64().unwrap(),
            vm["state"].as_str().unwrap().to_owned(),
        );
        // Each row carries the geometry pulled via vm_meta.
        assert!(vm["vcpus"].is_u64(), "row missing vcpus: {vm}");
        assert!(vm["memory_mib"].is_u64(), "row missing memory_mib: {vm}");
        assert!(
            vm["kernel_cmdline"].is_string(),
            "row missing kernel_cmdline: {vm}"
        );
    }
    assert_eq!(by_id[&ids[0]], "created");
    assert_eq!(by_id[&ids[1]], "running");
    assert_eq!(by_id[&ids[2]], "stopped");
}

#[tokio::test]
async fn list_vms_metadata_reflects_create_geometry() {
    let app = app();
    let (_, h) = send(
        app.clone(),
        Method::POST,
        "/v1/vms",
        Some(json!({
            "vcpus": 4,
            "memory_mib": 256,
            "cmdline": "console=ttyS0 panic=1",
        })),
    )
    .await;
    let id = h["id"].as_u64().unwrap();
    let (_, body) = send(app.clone(), Method::GET, "/v1/vms", None).await;
    let vms = body["vms"].as_array().unwrap();
    assert_eq!(vms.len(), 1);
    let vm = &vms[0];
    assert_eq!(vm["id"].as_u64().unwrap(), id);
    assert_eq!(vm["vcpus"], 4);
    assert_eq!(vm["memory_mib"], 256);
    assert_eq!(vm["kernel_cmdline"], "console=ttyS0 panic=1");
    // No snapshot_dir for cold-booted VMs — field omitted from JSON.
    assert!(vm.get("snapshot_dir").is_none() || vm["snapshot_dir"].is_null());
}

#[tokio::test]
async fn list_vms_excludes_destroyed_entries() {
    let app = app();
    let (_, a) = send(app.clone(), Method::POST, "/v1/vms", Some(json!({}))).await;
    let (_, b) = send(app.clone(), Method::POST, "/v1/vms", Some(json!({}))).await;
    let a_id = a["id"].as_u64().unwrap();
    let b_id = b["id"].as_u64().unwrap();
    send(
        app.clone(),
        Method::DELETE,
        &format!("/v1/vms/{a_id}"),
        None,
    )
    .await;
    let (_, body) = send(app.clone(), Method::GET, "/v1/vms", None).await;
    let vms = body["vms"].as_array().unwrap();
    assert_eq!(vms.len(), 1);
    assert_eq!(vms[0]["id"].as_u64().unwrap(), b_id);
}

// --- Extractor-rejection paths (must use the same error envelope). -----

#[tokio::test]
async fn malformed_json_body_uses_structured_error_envelope() {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/vms")
        .header("content-type", "application/json")
        .body(Body::from("{not valid json"))
        .unwrap();
    let resp = app().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("error body must be JSON");

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");
    assert!(body["error"]["message"].is_string());
}

#[tokio::test]
async fn missing_content_type_on_body_uses_structured_error_envelope() {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/vms")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("error body must be JSON");

    // axum rejects a missing/wrong content-type with 415 Unsupported Media
    // Type via JsonRejection; we just require that our envelope wraps it.
    assert!(status.is_client_error(), "status was {status}");
    assert_eq!(body["error"]["code"], "bad_request");
}

#[tokio::test]
async fn non_numeric_path_segment_uses_structured_error_envelope() {
    let (status, body) = send(app(), Method::GET, "/v1/vms/not-a-number", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");
}

// --- Bearer-token auth (`/v1/*` is guarded; `/healthz` is exempt). -----

#[tokio::test]
async fn empty_token_set_disables_auth() {
    let app = app_with_tokens(ApiTokens::default());
    let (status, _) = send(app, Method::POST, "/v1/vms", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn missing_authorization_header_returns_401() {
    let app = app_with_tokens(ApiTokens::new(["s3cret"]));
    let (status, body) = send(app, Method::POST, "/v1/vms", Some(json!({}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("missing"));
}

#[tokio::test]
async fn wrong_token_returns_401() {
    let app = app_with_tokens(ApiTokens::new(["s3cret"]));
    let (status, body) =
        send_with(app, Method::POST, "/v1/vms", Some(json!({})), Some("nope")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn valid_bearer_token_allows_request() {
    let app = app_with_tokens(ApiTokens::new(["s3cret"]));
    let (status, body) = send_with(
        app,
        Method::POST,
        "/v1/vms",
        Some(json!({})),
        Some("s3cret"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["state"], "created");
}

#[tokio::test]
async fn healthz_does_not_require_auth_even_when_tokens_are_set() {
    let app = app_with_tokens(ApiTokens::new(["s3cret"]));
    let (status, body) = send(app, Method::GET, "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::String("ok".into()));
}

#[tokio::test]
async fn openapi_json_does_not_require_auth_even_when_tokens_are_set() {
    let app = app_with_tokens(ApiTokens::new(["s3cret"]));
    let (status, body) = send(app, Method::GET, "/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["openapi"], "3.1.0");
}

#[tokio::test]
async fn malformed_authorization_header_returns_401() {
    let app = app_with_tokens(ApiTokens::new(["s3cret"]));
    // Missing the "Bearer " prefix.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/vms")
        .header("authorization", "s3cret")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn missing_api_tokens_extension_returns_structured_500() {
    // Mount the router WITHOUT an ApiTokens extension — simulates a library
    // consumer that forgot to `.layer(Extension(...))` before serving.
    let hv: Arc<dyn vm_core::Hypervisor> = Arc::new(MockHypervisor::new());
    let app: Router = router().with_state(AppState::new(hv));

    let (status, body) = send(app, Method::POST, "/v1/vms", Some(json!({}))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["code"], "internal");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("ApiTokens extension is missing"));
}

// --- POST /v1/vms with snapshot_dir ----------------------------------

/// Write a manifest into a fresh temp dir; caller cleans up.
fn snapshot_dir_with_manifest(slug: &str, snapshot_id: u64) -> std::path::PathBuf {
    use std::collections::BTreeMap;
    let dir = std::env::temp_dir().join(format!(
        "rust-nano-vm-cp-{}-{}-{}",
        slug,
        std::process::id(),
        snapshot_id
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut m = snapshot::Manifest::new(snapshot_id, 256 * 1024 * 1024, 4096, 4);
    m.kernel_cmdline = "console=ttyS0".into();
    m.labels = BTreeMap::from([("kind".into(), "test".into())]);
    m.write_to_dir(&dir).expect("write manifest");
    dir
}

#[tokio::test]
async fn create_vm_with_snapshot_dir_returns_created() {
    let dir = snapshot_dir_with_manifest("create-from-snapshot", 1);
    let body = json!({ "snapshot_dir": dir });
    let (status, resp) = send(app(), Method::POST, "/v1/vms", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "got body {resp:?}");
    assert_eq!(resp["state"], "created");
    assert!(resp["id"].is_u64());
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[tokio::test]
async fn create_vm_with_missing_snapshot_dir_returns_backend_500() {
    let body = json!({
        "snapshot_dir": "/nonexistent/rust-nano-vm/snapshot",
    });
    let (status, resp) = send(app(), Method::POST, "/v1/vms", Some(body)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    // Mapped from VmError::Backend(..) per error.rs.
    assert_eq!(resp["error"]["code"], "backend");
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("snapshot manifest"));
}

// --- GET /v1/snapshots, DELETE /v1/snapshots/:id --------------------

#[tokio::test]
async fn list_snapshots_is_empty_initially() {
    let (status, body) = send(app(), Method::GET, "/v1/snapshots", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["snapshots"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn list_snapshots_returns_each_captured_snapshot() {
    let app = app();
    let (_, h) = send(app.clone(), Method::POST, "/v1/vms", Some(json!({}))).await;
    let id = h["id"].as_u64().unwrap();
    send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/start"),
        None,
    )
    .await;
    // Capture two snapshots from the same VM.
    let (_, s1) = send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/snapshot"),
        None,
    )
    .await;
    let (_, s2) = send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/snapshot"),
        None,
    )
    .await;
    let s1_id = s1["id"].as_u64().unwrap();
    let s2_id = s2["id"].as_u64().unwrap();

    let (status, body) = send(app.clone(), Method::GET, "/v1/snapshots", None).await;
    assert_eq!(status, StatusCode::OK);
    let snaps = body["snapshots"].as_array().unwrap();
    assert_eq!(snaps.len(), 2);
    let mut ids: Vec<u64> = snaps.iter().map(|s| s["id"].as_u64().unwrap()).collect();
    ids.sort();
    let mut want = vec![s1_id, s2_id];
    want.sort();
    assert_eq!(ids, want);
    for s in snaps {
        assert!(s["display"].as_str().unwrap().starts_with("snap-"));
        // Each row carries the geometry pulled via snapshot_meta.
        assert!(s["vcpu_count"].is_u64(), "row missing vcpu_count: {s}");
        assert!(s["memory_bytes"].is_u64(), "row missing memory_bytes: {s}");
        assert!(s["page_size"].is_u64(), "row missing page_size: {s}");
        assert!(
            s["kernel_cmdline"].is_string(),
            "row missing kernel_cmdline: {s}"
        );
    }
}

#[tokio::test]
async fn list_snapshots_metadata_reflects_vm_geometry() {
    let app = app();
    // Create a VM with a recognizable geometry.
    let (_, h) = send(
        app.clone(),
        Method::POST,
        "/v1/vms",
        Some(json!({
            "vcpus": 4,
            "memory_mib": 64,
            "cmdline": "console=ttyS0 panic=1",
        })),
    )
    .await;
    let id = h["id"].as_u64().unwrap();
    send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/start"),
        None,
    )
    .await;
    send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/snapshot"),
        None,
    )
    .await;

    let (_, body) = send(app.clone(), Method::GET, "/v1/snapshots", None).await;
    let snaps = body["snapshots"].as_array().unwrap();
    assert_eq!(snaps.len(), 1);
    let s = &snaps[0];
    assert_eq!(s["vcpu_count"], 4);
    assert_eq!(s["memory_bytes"].as_u64().unwrap(), 64 * 1024 * 1024);
    assert_eq!(s["page_size"], 4096);
    assert_eq!(s["kernel_cmdline"], "console=ttyS0 panic=1");
}

#[tokio::test]
async fn delete_snapshot_removes_it_and_subsequent_restore_404s() {
    let app = app();
    let (_, h) = send(app.clone(), Method::POST, "/v1/vms", Some(json!({}))).await;
    let id = h["id"].as_u64().unwrap();
    send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/start"),
        None,
    )
    .await;
    let (_, s) = send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/snapshot"),
        None,
    )
    .await;
    let snap_id = s["id"].as_u64().unwrap();

    let (status, _) = send(
        app.clone(),
        Method::DELETE,
        &format!("/v1/snapshots/{snap_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Subsequent restore must 404 with the structured envelope.
    let (status, body) = send(
        app.clone(),
        Method::POST,
        &format!("/v1/snapshots/{snap_id}/restore"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "unknown_snapshot");

    // And the listing now omits it.
    let (_, list) = send(app.clone(), Method::GET, "/v1/snapshots", None).await;
    assert_eq!(list["snapshots"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn delete_unknown_snapshot_returns_structured_404() {
    let (status, body) = send(app(), Method::DELETE, "/v1/snapshots/99999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "unknown_snapshot");
}

#[tokio::test]
async fn snapshot_with_to_dir_persists_a_manifest_round_trip() {
    let app = app();

    // Create + start a VM with a recognizable cmdline so we can verify
    // that the persisted manifest captured it.
    let (_, h) = send(
        app.clone(),
        Method::POST,
        "/v1/vms",
        Some(json!({
            "vcpus": 4,
            "memory_mib": 64,
            "cmdline": "console=ttyS0 panic=1",
        })),
    )
    .await;
    let id = h["id"].as_u64().unwrap();
    send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/start"),
        None,
    )
    .await;

    let dir = std::env::temp_dir().join(format!(
        "rust-nano-vm-cp-persist-{}-{}",
        std::process::id(),
        id
    ));
    let _ = std::fs::remove_dir_all(&dir);

    let (status, body) = send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/snapshot"),
        Some(json!({ "to_dir": dir })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "got body {body:?}");
    assert!(body["display"].as_str().unwrap().starts_with("snap-"));
    assert_eq!(
        body["dir"].as_str().unwrap(),
        dir.to_str().unwrap(),
        "response must echo the persisted directory"
    );

    // The persisted manifest must be readable and contain the captured
    // geometry verbatim.
    let manifest = snapshot::Manifest::read_from_dir(&dir).expect("read manifest");
    assert_eq!(manifest.vcpu_count, 4);
    assert_eq!(manifest.memory_bytes, 64 * 1024 * 1024);
    assert_eq!(manifest.page_size, 4096);
    assert_eq!(manifest.kernel_cmdline, "console=ttyS0 panic=1");

    // Round-trip: a fresh `POST /v1/vms` with `snapshot_dir` referring
    // to the directory we just persisted accepts cleanly.
    let (status, _) = send(
        app.clone(),
        Method::POST,
        "/v1/vms",
        Some(json!({ "snapshot_dir": dir })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[tokio::test]
async fn snapshot_with_empty_body_still_works_legacy_shape() {
    // The old "no body, just capture in memory" shape must keep working
    // so we don't break existing callers.
    let app = app();
    let (_, h) = send(app.clone(), Method::POST, "/v1/vms", Some(json!({}))).await;
    let id = h["id"].as_u64().unwrap();
    send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/start"),
        None,
    )
    .await;
    let (status, body) = send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/snapshot"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(body["display"].as_str().unwrap().starts_with("snap-"));
    assert!(body.get("dir").is_none() || body["dir"].is_null());
}

// ---- Guest operations ---------------------------------------------------

/// Create a running VM and return its numeric id.
async fn create_running_vm(app: Router) -> u64 {
    let (_, h) = send(app.clone(), Method::POST, "/v1/vms", Some(json!({}))).await;
    let id = h["id"].as_u64().unwrap();
    send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/start"),
        None,
    )
    .await;
    id
}

#[tokio::test]
async fn exec_echo_returns_stdout_and_exit_code() {
    let app = app();
    let id = create_running_vm(app.clone()).await;

    let (status, body) = send(
        app,
        Method::POST,
        &format!("/v1/vms/{id}/exec"),
        Some(json!({ "program": "echo", "args": ["hello exec"] })),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "got body {body:?}");
    assert_eq!(body["exit_code"], 0);
    assert!(
        body["stdout"].as_str().unwrap().contains("hello exec"),
        "stdout was {:?}",
        body["stdout"]
    );
}

#[tokio::test]
async fn exec_reflects_non_zero_exit_code() {
    let app = app();
    let id = create_running_vm(app.clone()).await;

    let (status, body) = send(
        app,
        Method::POST,
        &format!("/v1/vms/{id}/exec"),
        Some(json!({ "program": "sh", "args": ["-c", "exit 42"] })),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "got body {body:?}");
    assert_eq!(body["exit_code"], 42);
}

#[tokio::test]
async fn exec_on_created_vm_returns_conflict() {
    let app = app();
    let (_, h) = send(app.clone(), Method::POST, "/v1/vms", Some(json!({}))).await;
    let id = h["id"].as_u64().unwrap();
    // VM is Created, not Running

    let (status, body) = send(
        app,
        Method::POST,
        &format!("/v1/vms/{id}/exec"),
        Some(json!({ "program": "echo", "args": [] })),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT, "got body {body:?}");
    assert_eq!(body["error"]["code"], "invalid_transition");
}

#[tokio::test]
async fn exec_unknown_vm_is_not_found() {
    let (status, body) = send(
        app(),
        Method::POST,
        "/v1/vms/99999/exec",
        Some(json!({ "program": "echo", "args": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "unknown_vm");
}

#[tokio::test]
async fn exec_missing_program_field_is_bad_request() {
    let app = app();
    let id = create_running_vm(app.clone()).await;

    let (status, body) = send(
        app,
        Method::POST,
        &format!("/v1/vms/{id}/exec"),
        Some(json!({ "args": ["hello"] })), // missing "program"
    )
    .await;

    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "got body {body:?}"
    );
    assert_eq!(body["error"]["code"], "bad_request");
}

#[tokio::test]
async fn write_and_read_file_roundtrip_via_http() {
    let app = app();
    let id = create_running_vm(app.clone()).await;

    let content: Vec<u8> = b"hello from http roundtrip".to_vec();
    let path = format!("/tmp/rust-nano-vm-api-test-{}-{}", std::process::id(), id);

    // Write
    let (status, body) = send(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{id}/files"),
        Some(json!({ "path": path, "content": content, "mode": 0o644u32 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "write: {body:?}");
    assert_eq!(body["bytes"].as_u64().unwrap(), content.len() as u64);

    // Read back
    let (status, body) = send(
        app.clone(),
        Method::GET,
        &format!("/v1/vms/{id}/files?path={path}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "read: {body:?}");
    let got: Vec<u8> = body["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_u64().unwrap() as u8)
        .collect();
    assert_eq!(got, content);

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn read_file_missing_path_query_is_bad_request() {
    let app = app();
    let id = create_running_vm(app.clone()).await;

    // No `?path=` query parameter
    let (status, body) = send(app, Method::GET, &format!("/v1/vms/{id}/files"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "got {body:?}");
}

#[tokio::test]
async fn read_file_nonexistent_path_returns_backend_error() {
    let app = app();
    let id = create_running_vm(app.clone()).await;

    let (status, body) = send(
        app,
        Method::GET,
        &format!("/v1/vms/{id}/files?path=/no/such/file/api/test"),
        None,
    )
    .await;
    // The mock surfaces a backend error (IO error on the host) which maps to 500.
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "got {body:?}");
    assert_eq!(body["error"]["code"], "backend");
}

// ---- /v1/snapshots/:id/fork + /v1/usage --------------------------------

async fn snapshot_a_fresh_vm(app: &Router, bearer: Option<&str>) -> u64 {
    let (_, vm) = send_with(
        app.clone(),
        Method::POST,
        "/v1/vms",
        Some(json!({})),
        bearer,
    )
    .await;
    let vm_id = vm["id"].as_u64().unwrap();
    let (status, snap) = send_with(
        app.clone(),
        Method::POST,
        &format!("/v1/vms/{vm_id}/snapshot"),
        None,
        bearer,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    snap["id"].as_u64().unwrap()
}

#[tokio::test]
async fn fork_returns_handle_with_latency_and_per_token_count() {
    let app = app_with_tokens(ApiTokens::from_csv("customer-alpha"));
    let snap = snapshot_a_fresh_vm(&app, Some("customer-alpha")).await;

    let (status, body) = send_with(
        app.clone(),
        Method::POST,
        &format!("/v1/snapshots/{snap}/fork"),
        None,
        Some("customer-alpha"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(body["vm"]["id"].is_u64(), "fork returns a new VM handle");
    assert!(body["fork_ms"].is_u64(), "fork carries latency in ms");
    assert_eq!(body["fork_count"], 1, "first fork bumps the counter to 1");
    assert!(body["fork_total_ms"].is_u64());
}

#[tokio::test]
async fn two_forks_accumulate_per_token() {
    let app = app_with_tokens(ApiTokens::from_csv("customer-alpha"));
    let snap = snapshot_a_fresh_vm(&app, Some("customer-alpha")).await;

    let path = format!("/v1/snapshots/{snap}/fork");
    let (_, a) = send_with(
        app.clone(),
        Method::POST,
        &path,
        None,
        Some("customer-alpha"),
    )
    .await;
    let (_, b) = send_with(
        app.clone(),
        Method::POST,
        &path,
        None,
        Some("customer-alpha"),
    )
    .await;
    assert_eq!(a["fork_count"], 1);
    assert_eq!(b["fork_count"], 2);
    assert!(
        b["fork_total_ms"].as_u64().unwrap() >= a["fork_total_ms"].as_u64().unwrap(),
        "fork_total_ms is monotonic"
    );
}

#[tokio::test]
async fn usage_endpoint_reports_per_token_counts() {
    let app = app_with_tokens(ApiTokens::from_csv("customer-alpha"));
    let snap = snapshot_a_fresh_vm(&app, Some("customer-alpha")).await;

    // Fork twice as alpha.
    let path = format!("/v1/snapshots/{snap}/fork");
    let _ = send_with(
        app.clone(),
        Method::POST,
        &path,
        None,
        Some("customer-alpha"),
    )
    .await;
    let _ = send_with(
        app.clone(),
        Method::POST,
        &path,
        None,
        Some("customer-alpha"),
    )
    .await;

    let (status, usage) = send_with(
        app.clone(),
        Method::GET,
        "/v1/usage",
        None,
        Some("customer-alpha"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(usage["fork_count"], 2);
    assert!(usage["fork_total_ms"].is_u64());
    // Fingerprint never leaks the raw bearer token.
    let token_field = usage["token"].as_str().unwrap();
    assert!(token_field.starts_with("tok-"));
    assert!(!token_field.contains("customer-alpha"));
}

#[tokio::test]
async fn fork_counts_are_isolated_between_tokens() {
    let app = app_with_tokens(ApiTokens::from_csv("alpha,beta"));
    let snap = snapshot_a_fresh_vm(&app, Some("alpha")).await;

    let path = format!("/v1/snapshots/{snap}/fork");
    let _ = send_with(app.clone(), Method::POST, &path, None, Some("alpha")).await;
    let _ = send_with(app.clone(), Method::POST, &path, None, Some("alpha")).await;
    let _ = send_with(app.clone(), Method::POST, &path, None, Some("beta")).await;

    let (_, ua) = send_with(app.clone(), Method::GET, "/v1/usage", None, Some("alpha")).await;
    let (_, ub) = send_with(app.clone(), Method::GET, "/v1/usage", None, Some("beta")).await;
    assert_eq!(ua["fork_count"], 2);
    assert_eq!(ub["fork_count"], 1);
}

#[tokio::test]
async fn usage_without_bearer_returns_unauthorized() {
    // Auth disabled (empty token set) — fork itself works, but /usage still
    // demands a bearer because counts are keyed on it.
    let app = app();
    let snap = snapshot_a_fresh_vm(&app, None).await;
    let (fork_status, _) = send(
        app.clone(),
        Method::POST,
        &format!("/v1/snapshots/{snap}/fork"),
        None,
    )
    .await;
    assert_eq!(fork_status, StatusCode::CREATED);

    let (usage_status, body) = send(app.clone(), Method::GET, "/v1/usage", None).await;
    assert_eq!(usage_status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
}

// ---- /v1/snapshots/:id/fork quota ---------------------------------------

/// Router with auth on AND a tight fork quota (1 fork burst, ~0 refill).
/// Used by quota tests so the second fork must 429.
fn app_with_tight_quota(tokens: ApiTokens) -> Router {
    use control_plane::ForkQuota;
    use std::sync::Arc as A;
    let hv: Arc<dyn vm_core::Hypervisor> = Arc::new(MockHypervisor::new());
    let quota = A::new(ForkQuota::new(0.001, 1)); // burst 1, ~no refill in test window
    control_plane::router()
        .layer(Extension(Arc::new(tokens)))
        .with_state(AppState::with_fork_quota(hv, quota))
}

async fn send_with_headers(
    app: Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    bearer: Option<&str>,
) -> (StatusCode, Value, axum::http::HeaderMap) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(tok) = bearer {
        builder = builder.header("authorization", format!("Bearer {tok}"));
    }
    let req = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&v).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, json, headers)
}

#[tokio::test]
async fn fork_returns_429_with_retry_after_when_quota_exhausted() {
    let app = app_with_tight_quota(ApiTokens::from_csv("alpha"));
    let snap = snapshot_a_fresh_vm(&app, Some("alpha")).await;
    let path = format!("/v1/snapshots/{snap}/fork");

    // First fork burns the burst.
    let (s1, _) = send_with(app.clone(), Method::POST, &path, None, Some("alpha")).await;
    assert_eq!(s1, StatusCode::CREATED);

    // Second fork in the same tick should be throttled.
    let (s2, body, headers) =
        send_with_headers(app.clone(), Method::POST, &path, None, Some("alpha")).await;
    assert_eq!(s2, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"]["code"], "fork_quota_exceeded");
    let retry = headers
        .get("retry-after")
        .expect("Retry-After header on 429")
        .to_str()
        .unwrap()
        .parse::<u64>()
        .expect("Retry-After parses as u64 seconds");
    assert!(retry >= 1, "Retry-After is at least 1 second");
}

#[tokio::test]
async fn fork_quota_is_per_token() {
    let app = app_with_tight_quota(ApiTokens::from_csv("alpha,beta"));
    let snap = snapshot_a_fresh_vm(&app, Some("alpha")).await;
    let path = format!("/v1/snapshots/{snap}/fork");

    let (sa, _) = send_with(app.clone(), Method::POST, &path, None, Some("alpha")).await;
    assert_eq!(sa, StatusCode::CREATED);

    // beta's bucket is untouched — should pass.
    let (sb, _) = send_with(app.clone(), Method::POST, &path, None, Some("beta")).await;
    assert_eq!(sb, StatusCode::CREATED);

    // alpha is now throttled.
    let (sa2, _) = send_with(app.clone(), Method::POST, &path, None, Some("alpha")).await;
    assert_eq!(sa2, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn fork_with_disabled_quota_passes_through() {
    use control_plane::ForkQuota;
    use std::sync::Arc as A;
    let hv: Arc<dyn vm_core::Hypervisor> = Arc::new(MockHypervisor::new());
    let quota = A::new(ForkQuota::new(0.0, 0)); // disabled
    let app = control_plane::router()
        .layer(Extension(Arc::new(ApiTokens::from_csv("alpha"))))
        .with_state(AppState::with_fork_quota(hv, quota));
    let snap = snapshot_a_fresh_vm(&app, Some("alpha")).await;
    let path = format!("/v1/snapshots/{snap}/fork");

    // 50 forks back-to-back, no throttling.
    for _ in 0..50 {
        let (status, _) = send_with(app.clone(), Method::POST, &path, None, Some("alpha")).await;
        assert_eq!(status, StatusCode::CREATED);
    }
}

// ---- /metrics ----------------------------------------------------------

#[tokio::test]
async fn metrics_endpoint_serves_prometheus_text_without_auth() {
    let app = app_with_tokens(ApiTokens::from_csv("alpha"));
    let (status, body, headers) =
        send_with_headers(app.clone(), Method::GET, "/metrics", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let ctype = headers.get("content-type").unwrap().to_str().unwrap();
    assert!(ctype.starts_with("text/plain"), "got {ctype}");
    // Body is the rendered Prometheus text; even with no activity we get
    // the heartbeat gauge and zero-valued latency lines.
    let text = match body {
        Value::String(s) => s,
        other => panic!("expected plain-text body, got JSON: {other}"),
    };
    assert!(text.contains("nanovm_up 1"));
    assert!(text.contains("nanovm_fork_latency_ms_count 0"));
}

#[tokio::test]
async fn metrics_records_successful_fork_per_token() {
    let app = app_with_tokens(ApiTokens::from_csv("alpha"));
    let snap = snapshot_a_fresh_vm(&app, Some("alpha")).await;
    let _ = send_with(
        app.clone(),
        Method::POST,
        &format!("/v1/snapshots/{snap}/fork"),
        None,
        Some("alpha"),
    )
    .await;

    let (status, body, _) = send_with_headers(app, Method::GET, "/metrics", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let text = match body {
        Value::String(s) => s,
        other => panic!("expected plain-text body: {other}"),
    };
    assert!(
        text.contains("nanovm_forks_total{token=\"tok-alph-5\"} 1"),
        "missing per-token fork counter:\n{text}"
    );
    assert!(text.contains("nanovm_fork_latency_ms_count 1"));
}

#[tokio::test]
async fn metrics_records_throttled_attempts() {
    let app = app_with_tight_quota(ApiTokens::from_csv("alpha"));
    let snap = snapshot_a_fresh_vm(&app, Some("alpha")).await;
    let path = format!("/v1/snapshots/{snap}/fork");

    // First fork drains the burst; second throttles.
    let _ = send_with(app.clone(), Method::POST, &path, None, Some("alpha")).await;
    let _ = send_with(app.clone(), Method::POST, &path, None, Some("alpha")).await;

    let (_, body, _) = send_with_headers(app, Method::GET, "/metrics", None, None).await;
    let text = match body {
        Value::String(s) => s,
        other => panic!("expected plain-text body: {other}"),
    };
    assert!(
        text.contains("nanovm_fork_quota_throttled_total{token=\"tok-alph-5\"} 1"),
        "missing throttle counter:\n{text}"
    );
    assert!(text.contains("nanovm_forks_total{token=\"tok-alph-5\"} 1"));
}

// ---- audit log -----------------------------------------------------------

/// Test-app builder that installs an [`AuditLog`] extension. The bearer-
/// token set is whatever the caller passes; pass `ApiTokens::default()` for
/// auth-disabled mode.
fn app_with_audit(tokens: ApiTokens, audit: control_plane::AuditLog) -> Router {
    let hv: Arc<dyn vm_core::Hypervisor> = Arc::new(MockHypervisor::new());
    router()
        .layer(Extension(Arc::new(tokens)))
        .layer(Extension(audit))
        .with_state(AppState::new(hv))
}

fn read_audit(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[tokio::test]
async fn audit_logs_authenticated_post_with_full_path() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let audit = control_plane::AuditLog::open(tmp.path()).unwrap();
    let tokens = ApiTokens::new(["secret-token"]);
    let app = app_with_audit(tokens, audit);

    let (status, _) = send_with(
        app,
        Method::POST,
        "/v1/vms",
        Some(json!({})),
        Some("secret-token"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let log = read_audit(tmp.path());
    let line = log.lines().next().expect("at least one audit line");
    let parsed: Value = serde_json::from_str(line).expect("audit line parses as JSON");
    assert_eq!(parsed["method"], "POST");
    assert_eq!(parsed["path"], "/v1/vms");
    assert_eq!(parsed["status"], 201);
    assert_eq!(parsed["token"], "tok-secr-12");
    assert!(parsed["ts"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test]
async fn audit_skips_get_requests() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let audit = control_plane::AuditLog::open(tmp.path()).unwrap();
    let tokens = ApiTokens::new(["t"]);
    let app = app_with_audit(tokens, audit);

    let (status, _) = send_with(app, Method::GET, "/v1/vms", None, Some("t")).await;
    assert_eq!(status, StatusCode::OK);

    let log = read_audit(tmp.path());
    assert!(log.is_empty(), "GETs must not be recorded; got: {log:?}");
}

#[tokio::test]
async fn audit_never_writes_raw_bearer_token() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let audit = control_plane::AuditLog::open(tmp.path()).unwrap();
    let raw = "very-secret-bearer-value"; // first4 = "very", len = 24
    let tokens = ApiTokens::new([raw]);
    let app = app_with_audit(tokens, audit);

    for _ in 0..3 {
        let _ = send_with(
            app.clone(),
            Method::POST,
            "/v1/vms",
            Some(json!({})),
            Some(raw),
        )
        .await;
    }

    let log = read_audit(tmp.path());
    assert!(
        !log.contains(raw),
        "raw bearer must never appear in audit log:\n{log}"
    );
    let expected_fp = format!("\"tok-very-{}\"", raw.len());
    assert_eq!(
        log.matches(&expected_fp).count(),
        3,
        "expected 3 fingerprinted lines (looking for {expected_fp}):\n{log}"
    );
}

#[tokio::test]
async fn audit_passes_through_when_extension_missing() {
    // No AuditLog extension installed; default `app()` helper.
    let (status, body) = send(app(), Method::POST, "/v1/vms", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(body["id"].is_u64());
}

#[tokio::test]
async fn audit_logs_unauthorized_attempt_is_rejected_before_audit() {
    // Auth on, audit on, wrong token. Auth rejects FIRST (outer layer), so
    // the audit file stays empty — that's the design.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let audit = control_plane::AuditLog::open(tmp.path()).unwrap();
    let tokens = ApiTokens::new(["good-token"]);
    let app = app_with_audit(tokens, audit);

    let (status, _) = send_with(
        app,
        Method::POST,
        "/v1/vms",
        Some(json!({})),
        Some("bad-token"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let log = read_audit(tmp.path());
    assert!(
        log.is_empty(),
        "auth-rejected requests must not appear in the audit log:\n{log:?}"
    );
}

#[tokio::test]
async fn audit_records_delete_and_status_code() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let audit = control_plane::AuditLog::open(tmp.path()).unwrap();
    let tokens = ApiTokens::new(["t"]);
    let app = app_with_audit(tokens, audit);

    // VmId::next() is a process-global counter, so the actual id of the
    // VM we create depends on what other parallel tests have done. Extract
    // it from the response rather than hard-coding 1.
    let (create_status, create_body) = send_with(
        app.clone(),
        Method::POST,
        "/v1/vms",
        Some(json!({})),
        Some("t"),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED);
    let vm_id = create_body["id"].as_u64().expect("create returned id");
    let delete_path = format!("/v1/vms/{vm_id}");

    let (status, _) = send_with(app, Method::DELETE, &delete_path, None, Some("t")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let log = read_audit(tmp.path());
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 audit lines, got: {log}");
    let first: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["method"], "POST");
    assert_eq!(first["status"], 201);
    let second: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["method"], "DELETE");
    assert_eq!(second["path"], delete_path);
    assert_eq!(second["status"], 204);
}

// ---- request-id ----------------------------------------------------------

#[tokio::test]
async fn request_id_is_minted_when_absent() {
    let app = app();
    let req = Request::builder()
        .method(Method::GET)
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let header = resp
        .headers()
        .get("x-request-id")
        .expect("response must carry x-request-id")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(header.starts_with("nanovm-"), "got {header}");
}

#[tokio::test]
async fn request_id_echoes_valid_inbound_header() {
    let app = app();
    let req = Request::builder()
        .method(Method::GET)
        .uri("/healthz")
        .header("x-request-id", "client-supplied.id_1-OK")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let echoed = resp
        .headers()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(echoed, "client-supplied.id_1-OK");
}

#[tokio::test]
async fn request_id_replaces_malicious_inbound_header() {
    let app = app();
    // CRLF injection would be a classic response-splitting attempt.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/healthz")
        .header("x-request-id", "evil; X-Injected: yes")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let echoed = resp
        .headers()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        echoed.starts_with("nanovm-"),
        "malicious id must be replaced with a freshly minted one, got: {echoed}"
    );
    // The attacker-controlled header value never appears in any other
    // response header.
    assert!(resp.headers().get("x-injected").is_none());
}

#[tokio::test]
async fn audit_record_includes_request_id() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let audit = control_plane::AuditLog::open(tmp.path()).unwrap();
    let tokens = ApiTokens::new(["t"]);
    let app = app_with_audit(tokens, audit);

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/vms")
        .header("authorization", "Bearer t")
        .header("content-type", "application/json")
        .header("x-request-id", "audit-corr-1")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap(),
        "audit-corr-1"
    );

    let log = read_audit(tmp.path());
    let line = log.lines().next().expect("at least one audit line");
    let parsed: Value = serde_json::from_str(line).expect("audit line parses as JSON");
    assert_eq!(parsed["request_id"], "audit-corr-1");
    assert_eq!(parsed["method"], "POST");
    assert_eq!(parsed["path"], "/v1/vms");
}
