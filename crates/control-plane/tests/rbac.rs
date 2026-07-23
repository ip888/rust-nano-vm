//! Role-based access-control enforcement tests.
//!
//! The `Role` middleware (`crates/control-plane/src/auth.rs`) is wired
//! onto the destructive/admin surface. These tests exercise the matrix
//! from three separately-scoped tokens (Viewer / Developer / Admin) and
//! assert both the allow and deny paths, so a future refactor that
//! accidentally drops the `require_role(...)` call on a handler blows
//! up here rather than in production.
//!
//! The env-var format is `org:token@role`. Legacy shapes without a role
//! suffix keep resolving to `Admin` (see [`Role::default_for_legacy`])
//! so a backward-compat test lives here too.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Extension, Router,
};
use control_plane::{router, ApiTokens, AppState};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use vm_mock::MockHypervisor;

/// Router with the three-role token set installed. Every RBAC test
/// starts from the same handful of tokens so the caller only picks
/// which one to present.
fn app_with_roles() -> Router {
    let tokens =
        ApiTokens::from_csv("acme:viewer-tok@viewer,acme:dev-tok@developer,acme:admin-tok@admin");
    let hv: Arc<dyn vm_core::Hypervisor> = Arc::new(MockHypervisor::new());
    router()
        .layer(Extension(Arc::new(tokens)))
        .with_state(AppState::new(hv))
}

async fn send(
    app: Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    bearer: &str,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {bearer}"));
    let req = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            builder
                .body(Body::from(serde_json::to_vec(&v).unwrap()))
                .unwrap()
        }
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

/// Assert a 403 response carries the machine-readable `role_required`
/// code so the SDK can differentiate role denials from ownership 403s.
fn assert_role_required(status: StatusCode, body: &Value) {
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "expected 403, got {status}, body: {body}"
    );
    assert_eq!(
        body.get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str()),
        Some("role_required"),
        "expected error.code = \"role_required\", body: {body}"
    );
}

// ---- POST /v1/keys (Admin) -----------------------------------------------

#[tokio::test]
async fn issue_key_denied_for_viewer() {
    let (status, body) = send(
        app_with_roles(),
        Method::POST,
        "/v1/keys",
        None,
        "viewer-tok",
    )
    .await;
    assert_role_required(status, &body);
}

#[tokio::test]
async fn issue_key_denied_for_developer() {
    let (status, body) = send(app_with_roles(), Method::POST, "/v1/keys", None, "dev-tok").await;
    assert_role_required(status, &body);
}

#[tokio::test]
async fn issue_key_allowed_for_admin() {
    let (status, body) = send(
        app_with_roles(),
        Method::POST,
        "/v1/keys",
        None,
        "admin-tok",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert!(body.get("token").and_then(|v| v.as_str()).is_some());
}

// ---- DELETE /v1/keys/:id (Admin) -----------------------------------------

#[tokio::test]
async fn revoke_key_denied_for_developer() {
    // Even a bogus id: role gate runs before path resolution so the
    // caller sees 403 not 404.
    let (status, body) = send(
        app_with_roles(),
        Method::DELETE,
        "/v1/keys/nvk_imaginary",
        None,
        "dev-tok",
    )
    .await;
    assert_role_required(status, &body);
}

#[tokio::test]
async fn revoke_key_denied_for_viewer() {
    let (status, body) = send(
        app_with_roles(),
        Method::DELETE,
        "/v1/keys/nvk_imaginary",
        None,
        "viewer-tok",
    )
    .await;
    assert_role_required(status, &body);
}

#[tokio::test]
async fn revoke_key_admin_gets_past_role_check() {
    // Admin bypasses the role gate; the handler then 404s the unknown id.
    let (status, _) = send(
        app_with_roles(),
        Method::DELETE,
        "/v1/keys/nvk_imaginary",
        None,
        "admin-tok",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---- GET /v1/keys (all roles) --------------------------------------------

#[tokio::test]
async fn list_keys_allowed_for_viewer() {
    // Read-only access — every authenticated role can list.
    let (status, _) = send(
        app_with_roles(),
        Method::GET,
        "/v1/keys",
        None,
        "viewer-tok",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// ---- DELETE /v1/vms/:id (Developer or higher) ----------------------------

#[tokio::test]
async fn destroy_vm_denied_for_viewer() {
    let (status, body) = send(
        app_with_roles(),
        Method::DELETE,
        "/v1/vms/1",
        None,
        "viewer-tok",
    )
    .await;
    assert_role_required(status, &body);
}

#[tokio::test]
async fn destroy_vm_developer_gets_past_role_check() {
    // Developer passes the role gate; ownership/hypervisor then rejects
    // an unknown vm id. We don't care whether the follow-up is 404 or
    // 403-owner-mismatch — only that it's NOT the role-required 403.
    let (status, body) = send(
        app_with_roles(),
        Method::DELETE,
        "/v1/vms/9999",
        None,
        "dev-tok",
    )
    .await;
    if status == StatusCode::FORBIDDEN {
        // If we got a 403, make sure it's ownership-shaped, not role.
        assert_ne!(
            body.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str()),
            Some("role_required"),
            "developer was blocked on role check, not ownership: {body}"
        );
    } else {
        // Otherwise 404 (unknown vm) is the expected shape.
        assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    }
}

// ---- DELETE /v1/snapshots/:id (Developer or higher) ----------------------

#[tokio::test]
async fn delete_snapshot_denied_for_viewer() {
    let (status, body) = send(
        app_with_roles(),
        Method::DELETE,
        "/v1/snapshots/1",
        None,
        "viewer-tok",
    )
    .await;
    assert_role_required(status, &body);
}

#[tokio::test]
async fn delete_snapshot_developer_gets_past_role_check() {
    let (status, body) = send(
        app_with_roles(),
        Method::DELETE,
        "/v1/snapshots/9999",
        None,
        "dev-tok",
    )
    .await;
    if status == StatusCode::FORBIDDEN {
        assert_ne!(
            body.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str()),
            Some("role_required"),
            "developer was blocked on role check, not ownership: {body}"
        );
    } else {
        assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    }
}

// ---- GET /v1/vms, GET /v1/snapshots (all roles) --------------------------

#[tokio::test]
async fn list_vms_allowed_for_viewer() {
    let (status, _) = send(app_with_roles(), Method::GET, "/v1/vms", None, "viewer-tok").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn list_snapshots_allowed_for_viewer() {
    let (status, _) = send(
        app_with_roles(),
        Method::GET,
        "/v1/snapshots",
        None,
        "viewer-tok",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// ---- Backward-compat: legacy tokens with no @role default to Admin -------

#[tokio::test]
async fn legacy_token_no_role_suffix_still_admin() {
    // The pre-RBAC env format `org:token` (no @role) MUST keep resolving
    // to Admin — the whole point of the shovel-ready stub was that
    // legacy deployments continue working byte-for-byte after
    // enforcement lands.
    let tokens = ApiTokens::from_csv("acme:legacy-tok");
    let hv: Arc<dyn vm_core::Hypervisor> = Arc::new(MockHypervisor::new());
    let app: Router = router()
        .layer(Extension(Arc::new(tokens)))
        .with_state(AppState::new(hv));

    // Legacy caller can mint a key (Admin-only route).
    let (status, body) = send(app, Method::POST, "/v1/keys", None, "legacy-tok").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "legacy no-suffix token should default to Admin; body: {body}"
    );
}

#[tokio::test]
async fn empty_token_set_disables_auth_and_grants_admin() {
    // With no tokens configured (auth-disabled dev mode), the middleware
    // injects Admin so every handler stays reachable — otherwise every
    // local `cargo run` deployment would break on the destructive
    // routes.
    let hv: Arc<dyn vm_core::Hypervisor> = Arc::new(MockHypervisor::new());
    let app: Router = router()
        .layer(Extension(Arc::new(ApiTokens::default())))
        .with_state(AppState::new(hv));

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/keys")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}
