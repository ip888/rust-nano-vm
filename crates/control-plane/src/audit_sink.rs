//! Enterprise-tier SIEM sink for the [`crate::audit::AuditLog`].
//!
//! When `NANOVM_AUDIT_SINK_URL` is set (and the binary was built with
//! `--features audit-sink`), a background tokio task drains a bounded
//! mpsc channel and POSTs each audit record as its own JSON body to the
//! configured URL. Every mutating `/v1/*` call therefore lands in both
//! the operator's local JSONL file (durable, sortable) AND the
//! customer's SIEM (Datadog / Splunk HEC / any HTTPS collector) — same
//! record, two destinations, independent failure modes.
//!
//! ## Design constraints
//!
//! - **Best-effort.** SIEM ingestion isn't the source of truth; a
//!   dropped record must not affect request handling. Full channel →
//!   log warn + drop the record; POST failure → log warn + drop.
//! - **Bounded backpressure.** The mpsc channel is capped
//!   ([`SINK_CHANNEL_CAP`]) so a slow / down sink can't grow memory
//!   without bound. First N records buffer, past that we drop-newest
//!   with a metric — better than pausing request threads waiting for a
//!   dead collector.
//! - **Zero request-latency impact.** [`AuditLog::append`] pushes
//!   into the channel via `try_send`, which is O(1) and never awaits.
//!   The HTTP POST happens entirely on the background task.
//! - **No auth secrets in logs.** The optional
//!   `NANOVM_AUDIT_SINK_HEADER` value (e.g. `DD-API-KEY: xxx`) never
//!   appears in tracing output — only the header name.

use std::time::Duration;

use axum::http::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Client;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Env var pointing at the SIEM ingest URL. HTTPS strongly preferred;
/// `http://` works for local collectors and logs a warn on startup.
pub const SINK_URL_ENV: &str = "NANOVM_AUDIT_SINK_URL";

/// Optional env var carrying a single extra header sent with every POST.
/// Format: `Name: Value` (e.g. `DD-API-KEY: abc123` for Datadog HTTP
/// intake, `Authorization: Bearer …` for a generic collector). The
/// value is passed verbatim; only the name appears in log lines.
pub const SINK_HEADER_ENV: &str = "NANOVM_AUDIT_SINK_HEADER";

/// Capacity of the mpsc channel between [`AuditLog::append`] and the
/// background sink task. 1024 slots is ~a few seconds of typical
/// mutating-request traffic; if the sink is down for longer than that,
/// we start dropping records with a warn (which is the point — better
/// than growing memory unbounded).
const SINK_CHANNEL_CAP: usize = 1024;

/// Per-request timeout on the HTTP POST to the collector. Keep tight —
/// a slow sink shouldn't back up the channel drain loop. There is NO
/// per-record retry: a failed POST logs a warn and the record is
/// dropped. The next audit event triggers a fresh POST — if the
/// collector has recovered, shipping resumes; the dropped one stays
/// lost. Best-effort by design.
const SINK_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Successfully-parsed sink config. `sender` is what [`AuditLog::append`]
/// pushes into; the [`JoinHandle`] is owned by the caller so the drain
/// task lives as long as the server does.
#[derive(Debug)]
pub struct SinkHandle {
    /// Channel `AuditLog` clones into itself for background dispatch.
    pub sender: mpsc::Sender<Value>,
    /// Drain task — spawned onto the tokio runtime by [`spawn`]. Kept
    /// alive by holding this handle; drop it to stop draining (mostly
    /// for tests — production keeps the handle for process lifetime).
    pub task: JoinHandle<()>,
}

/// Parse the sink-header env value (`Name: Value`) into a
/// (HeaderName, HeaderValue) pair. Whitespace around either side is
/// trimmed. Malformed values are rejected — the header is dropped and
/// the caller decides whether to boot or fail.
///
/// Never logs the header VALUE — only the name — so an API key in
/// `NANOVM_AUDIT_SINK_HEADER` doesn't end up in stdout.
pub fn parse_header(raw: &str) -> Result<(HeaderName, HeaderValue), String> {
    let (name, value) = raw
        .split_once(':')
        .ok_or_else(|| "expected `Name: Value`, got no colon".to_string())?;
    let name = name.trim();
    let value = value.trim();
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|e| format!("invalid header name {name:?}: {e}"))?;
    let value = HeaderValue::from_str(value).map_err(|e| format!("invalid header value: {e}"))?;
    Ok((name, value))
}

/// Spawn the background drain task and return the sender + handle.
/// Called from the server binary during startup after reading the
/// sink URL + optional header from env.
///
/// The task loops on `rx.recv().await`: on each record it POSTs to
/// `url` with `Content-Type: application/json`. On any error (network,
/// non-2xx status, timeout) it logs a warn and moves on — no retry.
/// When the sender is dropped (server shutdown), the loop exits
/// cleanly.
pub fn spawn(url: String, extra_header: Option<(HeaderName, HeaderValue)>) -> SinkHandle {
    let (tx, mut rx) = mpsc::channel::<Value>(SINK_CHANNEL_CAP);
    let client = build_client(extra_header);
    let task = tokio::spawn(async move {
        if url.starts_with("http://") {
            tracing::warn!(
                url = %redact_url(&url),
                "NANOVM_AUDIT_SINK_URL uses http:// — audit records will be sent unencrypted"
            );
        }
        // Client-build failure is a single-shot startup error: log it
        // ONCE and drain the channel until it closes, dropping every
        // record. Previously the log fired per-record, producing a
        // log storm indistinguishable from a live-but-failing sink.
        let client = match client {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "audit sink: HTTP client init failed at startup; all records will be dropped for the process lifetime"
                );
                // Drain the channel so the sender-side try_send
                // doesn't accumulate a full backlog either.
                while rx.recv().await.is_some() {}
                tracing::info!("audit sink: channel closed, drain task exiting");
                return;
            }
        };
        while let Some(record) = rx.recv().await {
            post_one(&client, &url, &record).await;
        }
        tracing::info!("audit sink: channel closed, drain task exiting");
    });
    SinkHandle { sender: tx, task }
}

/// Strip userinfo + query-string from a URL for safe logging. A
/// misconfigured `NANOVM_AUDIT_SINK_URL` like
/// `https://user:secret@collector.example.com/ingest?token=xyz` would
/// otherwise leak credentials into stdout the moment the http:// warn
/// fires. Best-effort: on unparseable URLs, log the scheme + `<redacted>`
/// so the operator still gets a signal without the secret.
fn redact_url(raw: &str) -> String {
    // We don't have `url` in deps; do a lightweight parse: keep the
    // scheme+host+path, drop userinfo and query.
    let (scheme, rest) = match raw.split_once("://") {
        Some(sr) => sr,
        None => return "<redacted>".to_string(),
    };
    // Split off the query first.
    let path = rest.split('?').next().unwrap_or("");
    // Strip userinfo if present: everything before an `@` that appears
    // before the first `/` is auth material.
    let host_and_path = match path.split_once('@') {
        Some((_userinfo, rest)) if !rest.contains('/') || _userinfo.chars().all(|c| c != '/') => {
            rest
        }
        _ => path,
    };
    format!("{scheme}://{host_and_path}")
}

/// Build the shared reqwest client. Broken out so a client-build
/// failure can be logged once and every subsequent POST short-circuits
/// (rather than rebuilding + failing on every record).
fn build_client(extra_header: Option<(HeaderName, HeaderValue)>) -> Result<Client, reqwest::Error> {
    let mut default_headers = HeaderMap::new();
    if let Some((name, value)) = extra_header {
        default_headers.insert(name, value);
    }
    Client::builder()
        .timeout(SINK_HTTP_TIMEOUT)
        .default_headers(default_headers)
        .build()
}

async fn post_one(client: &Client, url: &str, record: &Value) {
    let resp = client.post(url).json(record).send().await;
    match resp {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            tracing::warn!(
                status = r.status().as_u16(),
                "audit sink: POST returned non-2xx; dropping record"
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "audit sink: POST failed; dropping record");
        }
    }
}

/// Non-blocking push. Called from the request-hot-path in
/// [`AuditLog::append`]; must return immediately regardless of sink
/// backpressure. Full channel → drop the record and log a warn per
/// drop.
///
/// The warn is **not rate-limited** today — a truly-down sink at a
/// high mutating-request rate will produce warn-per-request until the
/// sink recovers or the operator drops the log level for this target.
/// If that turns out to be a problem in the wild, wire a token-bucket
/// or a "first + every 100th drop" filter here. Left honest for now.
///
/// Split out so the AuditLog side stays a plain sync `Sender::try_send`
/// call without importing the mpsc type directly.
pub fn push(sender: &mpsc::Sender<Value>, record: Value) {
    if let Err(err) = sender.try_send(record) {
        match err {
            mpsc::error::TrySendError::Full(_) => {
                tracing::warn!(
                    cap = SINK_CHANNEL_CAP,
                    "audit sink: channel full, dropping record (sink may be down)"
                );
            }
            mpsc::error::TrySendError::Closed(_) => {
                tracing::warn!("audit sink: channel closed, dropping record");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Router};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    // ---- parse_header ------------------------------------------------

    #[test]
    fn parse_header_happy_path() {
        let (name, value) = parse_header("DD-API-KEY: abc123").unwrap();
        assert_eq!(name.as_str(), "dd-api-key");
        assert_eq!(value.to_str().unwrap(), "abc123");
    }

    #[test]
    fn parse_header_trims_whitespace() {
        let (name, value) = parse_header("  Authorization  :  Bearer xyz  ").unwrap();
        assert_eq!(name.as_str(), "authorization");
        assert_eq!(value.to_str().unwrap(), "Bearer xyz");
    }

    #[test]
    fn parse_header_rejects_no_colon() {
        let err = parse_header("just-a-key").unwrap_err();
        assert!(err.contains("no colon"), "got: {err}");
    }

    #[test]
    fn parse_header_rejects_control_char_in_value() {
        // \n in a header value is a smuggling risk; reqwest / http crate
        // rejects it.
        let err = parse_header("X-Test: bad\nvalue").unwrap_err();
        assert!(err.contains("invalid header value"), "got: {err}");
    }

    #[test]
    fn redact_url_strips_userinfo_and_query() {
        // Plain HTTPS host: unchanged (scheme+host+path preserved,
        // query dropped).
        assert_eq!(
            redact_url("https://collector.example.com/ingest"),
            "https://collector.example.com/ingest"
        );
        // Userinfo present: dropped.
        assert_eq!(
            redact_url("https://user:secret@collector.example.com/ingest"),
            "https://collector.example.com/ingest"
        );
        // Query with token: dropped so it doesn't hit stdout.
        assert_eq!(
            redact_url("https://collector.example.com/ingest?token=abc"),
            "https://collector.example.com/ingest"
        );
        // Both userinfo AND query: both dropped.
        assert_eq!(
            redact_url("https://x:y@collector.example.com/ingest?t=abc"),
            "https://collector.example.com/ingest"
        );
        // Malformed input: returns the redacted sentinel so operators
        // still get a startup signal without the raw value.
        assert_eq!(redact_url("not a url"), "<redacted>");
    }

    // ---- end-to-end drain into a local test server -------------------

    /// Spin up an axum test server that records incoming JSON bodies.
    async fn serve_capture(
        received: Arc<Mutex<Vec<Value>>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/ingest",
            post(move |body: axum::Json<Value>| {
                let received = received.clone();
                async move {
                    received.lock().unwrap().push(body.0);
                    axum::http::StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/ingest"), handle)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end_records_reach_sink() {
        let received: Arc<Mutex<Vec<Value>>> = Arc::default();
        let (url, _server) = serve_capture(received.clone()).await;

        let sink = spawn(url, None);
        push(
            &sink.sender,
            json!({"ts":"2026-07-21T00:00:00.000Z","method":"POST","path":"/v1/vms"}),
        );
        push(
            &sink.sender,
            json!({"ts":"2026-07-21T00:00:01.000Z","method":"DELETE","path":"/v1/vms/1"}),
        );

        // Give the drain task a moment. This is a poll rather than a
        // fixed sleep so slow CI doesn't flake.
        for _ in 0..40 {
            if received.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let got = received.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            2,
            "both records should have arrived, got {got:?}"
        );
        assert_eq!(got[0]["path"], "/v1/vms");
        assert_eq!(got[1]["path"], "/v1/vms/1");

        drop(sink.sender);
        // The task exits on channel close; joining it makes sure we
        // haven't leaked a lingering worker.
        // Assert the drain task exits cleanly. A `Err` (timeout) or
        // an `Ok(Err(_))` (task panicked) would silently pass without
        // this — precisely the failure mode Copilot flagged.
        let join = tokio::time::timeout(Duration::from_secs(1), sink.task)
            .await
            .expect("drain task should exit within 1s after sender drop");
        join.expect("drain task must not panic");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sink_drops_on_non_2xx_without_stalling() {
        // Collector that always 500s. Sink must still consume records
        // without piling up or panicking.
        let app = Router::new().route(
            "/broken",
            post(|_body: axum::Json<Value>| async {
                axum::http::StatusCode::INTERNAL_SERVER_ERROR
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let url = format!("http://{addr}/broken");

        let sink = spawn(url, None);
        for i in 0..5 {
            push(
                &sink.sender,
                json!({"ts":"2026-07-21T00:00:00.000Z","path":format!("/v1/x/{i}")}),
            );
        }
        // Wait long enough for the drain loop to consume all 5. If a
        // 500 poisoned the task, we'd hit the timeout.
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Sender should still be usable (task is alive).
        assert!(!sink.sender.is_closed());
        drop(sink.sender);
        // Assert the drain task exits cleanly. A `Err` (timeout) or
        // an `Ok(Err(_))` (task panicked) would silently pass without
        // this — precisely the failure mode Copilot flagged.
        let join = tokio::time::timeout(Duration::from_secs(1), sink.task)
            .await
            .expect("drain task should exit within 1s after sender drop");
        join.expect("drain task must not panic");
    }
}
