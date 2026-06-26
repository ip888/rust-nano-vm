//! End-to-end integration test: spawn an in-process axum mock that
//! mimics the control plane's `POST /v1/sandbox/invoke` and exercise
//! the `SandboxClient` against it. Confirms the wire shape sent
//! (action discriminator + per-action fields), the bearer header,
//! the bridge-side snapshot fallback, the response parse, and the
//! HTTP-error path.
//!
//! No real KVM, no real control plane — the test is fast and
//! deterministic, runs on any host with cargo.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use nanovm_mcp::{client::Config, client::SandboxClient};

/// Captures every request body the mock received, in order, so a
/// test can assert on what the bridge sent.
type Captured = Arc<Mutex<Vec<CapturedRequest>>>;

#[derive(Debug, Clone)]
struct CapturedRequest {
    body: Value,
    bearer: Option<String>,
}

/// Spawn the mock on a random local port and return its base URL +
/// the shared capture list.
async fn spawn_mock(canned: Value, status: u16) -> (String, Captured) {
    use axum::{
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::post,
        Json, Router,
    };

    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let state = MockState {
        captured: Arc::clone(&captured),
        canned: Arc::new(canned),
        status,
    };

    let app = Router::new()
        .route(
            "/v1/sandbox/invoke",
            post(
                |State(s): State<MockState>,
                 headers: HeaderMap,
                 Json(body): Json<Value>| async move {
                    let bearer = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer ").map(|s| s.to_owned()));
                    s.captured.lock().unwrap().push(CapturedRequest { body, bearer });
                    let code = StatusCode::from_u16(s.status).unwrap();
                    (code, Json((*s.canned).clone()))
                },
            ),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), captured)
}

#[derive(Clone)]
struct MockState {
    captured: Captured,
    canned: Arc<Value>,
    status: u16,
}

fn make_client(base_url: &str, token: Option<&str>, snapshot: Option<u64>) -> SandboxClient {
    let cfg = Config {
        base_url: base_url.to_owned(),
        token: token.map(str::to_owned),
        snapshot,
    };
    SandboxClient::new(cfg).unwrap()
}

#[tokio::test]
async fn invoke_sends_action_body_and_parses_canonical_envelope() {
    let canned = json!({
        "stdout": "hello\n",
        "stderr": "",
        "exit_code": 0,
        "duration_ms": 17,
        "cold_start": true
    });
    let (url, captured) = spawn_mock(canned, 200).await;
    let client = make_client(&url, None, None);

    let result = client
        .invoke(json!({"action": "execute_shell", "command": "echo hello"}))
        .await
        .unwrap();

    assert_eq!(result.stdout, "hello\n");
    assert_eq!(result.exit_code, 0);
    assert!(result.cold_start);

    let cap = captured.lock().unwrap();
    assert_eq!(cap.len(), 1);
    assert_eq!(cap[0].body["action"], "execute_shell");
    assert_eq!(cap[0].body["command"], "echo hello");
    assert!(cap[0].bearer.is_none(), "no token configured");
}

#[tokio::test]
async fn invoke_sends_bearer_when_token_configured() {
    let canned = json!({
        "stdout": "", "stderr": "", "exit_code": 0,
        "duration_ms": 1, "cold_start": false
    });
    let (url, captured) = spawn_mock(canned, 200).await;
    let client = make_client(&url, Some("secret-token"), None);

    client
        .invoke(json!({"action": "execute_shell", "command": "true"}))
        .await
        .unwrap();

    let cap = captured.lock().unwrap();
    assert_eq!(cap[0].bearer.as_deref(), Some("secret-token"));
}

#[tokio::test]
async fn invoke_merges_bridge_snapshot_default_when_caller_omits_it() {
    let canned = json!({
        "stdout": "", "stderr": "", "exit_code": 0,
        "duration_ms": 1, "cold_start": false
    });
    let (url, captured) = spawn_mock(canned, 200).await;
    let client = make_client(&url, None, Some(99));

    // Caller passes no `snapshot` — bridge fills in 99.
    client
        .invoke(json!({"action": "list_files", "path": "/"}))
        .await
        .unwrap();

    let cap = captured.lock().unwrap();
    assert_eq!(cap[0].body["snapshot"], 99);
}

#[tokio::test]
async fn invoke_does_not_overwrite_caller_supplied_snapshot() {
    let canned = json!({
        "stdout": "", "stderr": "", "exit_code": 0,
        "duration_ms": 1, "cold_start": false
    });
    let (url, captured) = spawn_mock(canned, 200).await;
    let client = make_client(&url, None, Some(99));

    // Caller pinned snapshot=7 — bridge default must NOT clobber.
    client
        .invoke(json!({"action": "list_files", "path": "/", "snapshot": 7}))
        .await
        .unwrap();

    let cap = captured.lock().unwrap();
    assert_eq!(cap[0].body["snapshot"], 7);
}

#[tokio::test]
async fn invoke_surfaces_http_status_and_body_on_non_2xx() {
    let err_envelope = json!({
        "error": {"code": "unknown_snapshot", "message": "no such snapshot"}
    });
    let (url, _) = spawn_mock(err_envelope, 404).await;
    let client = make_client(&url, None, None);

    let err = client
        .invoke(json!({"action": "execute_shell", "command": "true", "snapshot": 99999}))
        .await
        .unwrap_err();

    match err {
        nanovm_mcp::client::InvokeError::Http { status, body } => {
            assert_eq!(status, 404);
            assert!(body.contains("unknown_snapshot"));
        }
        other => panic!("expected Http error, got {other:?}"),
    }
}
