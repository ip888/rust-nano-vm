//! JSONL audit log of mutating `/v1/*` calls.
//!
//! When the operator sets the `NANOVM_AUDIT_LOG` environment variable to a
//! filesystem path, the control plane appends one JSON object per mutating
//! request (POST / PUT / PATCH / DELETE) under `/v1/*` to that file:
//!
//! ```jsonl
//! {"ts":"2026-06-22T00:00:00.123Z","method":"POST","path":"/v1/vms","status":201,"token":"tok-abcd-12"}
//! ```
//!
//! Properties:
//!
//! - **The raw bearer never appears.** The `token` field carries the
//!   non-cryptographic fingerprint `tok-<first4>-<len>` — enough to
//!   correlate "all these were the same caller" in a leak investigation,
//!   but reveals nothing useful about the secret. Read calls are logged
//!   under the same fingerprint they would be charged under in `/v1/usage`.
//! - **Only mutating verbs are recorded.** `GET` requests are skipped: in a
//!   regulated deployment, the high-value audit trail is "who changed what",
//!   not "who looked". (Add request-id correlation if you also need access
//!   tracking.)
//! - **Layer ordering matters.** The middleware is registered *inside*
//!   `require_token` so unauthenticated requests are rejected before they
//!   reach the appender. The full request path is recovered via
//!   `OriginalUri` because `req.uri()` inside a `route_layer` on a nested
//!   sub-router has the `/v1` prefix stripped.
//! - **Write failures are best-effort.** A full disk or unwritable file
//!   logs a `tracing::error!` once per failed append and the request still
//!   completes with its real status. Losing one audit line is preferable to
//!   losing every request after the disk fills.
//! - **Rotation pattern.** The binary keeps an append handle. Rotate via
//!   `logrotate` with `copytruncate` so the inode stays stable.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Extension, OriginalUri, Request},
    http::Method,
    middleware::Next,
    response::Response,
};
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::request_id::RequestId;
use crate::routes::token_fingerprint;
use crate::time::rfc3339_now;

/// JSONL audit appender. Cheap to clone — wraps an `Arc<Mutex<File>>`.
#[derive(Clone, Debug, Default)]
pub struct AuditLog {
    inner: Option<Arc<Mutex<File>>>,
    path: Option<PathBuf>,
}

impl AuditLog {
    /// Disabled appender. `append` is a no-op.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Open `path` for append, creating it if it doesn't exist. Returns
    /// `Err(io)` if the path is unwritable.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            inner: Some(Arc::new(Mutex::new(file))),
            path: Some(path),
        })
    }

    /// Build from the `NANOVM_AUDIT_LOG` environment variable. When unset
    /// or empty, returns a disabled appender. When the path can't be
    /// opened for append, logs an `ERROR` and returns a disabled appender
    /// (the binary still boots — preferable to refusing service over a
    /// log-config mistake).
    pub fn from_env() -> Self {
        let raw = std::env::var("NANOVM_AUDIT_LOG").unwrap_or_default();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Self::disabled();
        }
        match Self::open(trimmed) {
            Ok(log) => log,
            Err(err) => {
                tracing::error!(
                    path = trimmed,
                    %err,
                    "NANOVM_AUDIT_LOG is set but the file can't be opened; \
                     continuing with the audit log disabled"
                );
                Self::disabled()
            }
        }
    }

    /// `true` when no audit destination is configured.
    pub fn is_disabled(&self) -> bool {
        self.inner.is_none()
    }

    /// Filesystem path the appender writes to, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Append a record. Write failures are logged once and dropped — the
    /// caller's request still completes.
    pub fn append(&self, record: &Value) {
        let Some(file) = &self.inner else {
            return;
        };
        let mut line = match serde_json::to_vec(record) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::error!(%err, "audit record failed to serialize");
                return;
            }
        };
        line.push(b'\n');
        let mut guard = match file.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Err(err) = guard.write_all(&line) {
            tracing::error!(%err, "audit log append failed");
        }
    }
}

/// axum middleware: append one JSONL record per mutating `/v1/*` call.
///
/// Layer ordering — the call site MUST register this middleware *inside*
/// `require_token` (i.e. earlier in the `.route_layer` chain) so the audit
/// log only captures authenticated requests. Unauthenticated ones are
/// rejected by auth before they reach this middleware.
///
/// `GET` requests are skipped: the value of an audit log is "who changed
/// what", and noise from read traffic would crowd it out. If the
/// `AuditLog` extension is missing (library consumer didn't install it),
/// the middleware passes through unchanged — same degrade-gracefully shape
/// as the rate limiter and metrics.
pub async fn require_audit(
    audit: Option<Extension<AuditLog>>,
    OriginalUri(original_uri): OriginalUri,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let Some(Extension(audit)) = audit else {
        return Ok(next.run(req).await);
    };
    if audit.is_disabled() {
        return Ok(next.run(req).await);
    }
    if !is_mutating(req.method()) {
        return Ok(next.run(req).await);
    }
    let method = req.method().clone();
    let path = original_uri.path().to_owned();
    let bearer = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    let token_label = bearer
        .map(token_fingerprint)
        .unwrap_or_else(|| "anonymous".to_owned());
    // request-id is installed by the outermost middleware. If it's missing
    // (e.g. library consumer composed router differently) we still log,
    // just without the correlation field.
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .map(|r| r.as_str().to_owned());
    let response = next.run(req).await;
    let status = response.status().as_u16();
    let mut record = json!({
        "ts": rfc3339_now(),
        "method": method.as_str(),
        "path": path,
        "status": status,
        "token": token_label,
    });
    if let Some(rid) = request_id {
        record["request_id"] = Value::String(rid);
    }
    audit.append(&record);
    Ok(response)
}

fn is_mutating(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    // The RFC 3339 / civil-date helpers moved to `crate::time`; their
    // shape tests live there now. Tests below cover the appender itself
    // and the require_audit middleware indirectly through integration
    // tests in `tests/api.rs`.

    #[test]
    fn disabled_append_is_a_noop() {
        let log = AuditLog::disabled();
        log.append(&json!({"hello": "world"}));
        assert!(log.is_disabled());
    }

    #[test]
    fn append_writes_a_line() {
        let tmp = NamedTempFile::new().unwrap();
        let log = AuditLog::open(tmp.path()).unwrap();
        log.append(&json!({"a": 1}));
        log.append(&json!({"b": 2}));
        let mut s = String::new();
        std::fs::File::open(tmp.path())
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "{\"a\":1}\n{\"b\":2}\n");
    }

    #[test]
    fn from_env_unset_is_disabled() {
        // SAFETY: tests don't share env across threads in cargo test default,
        // but be paranoid anyway.
        let prev = std::env::var("NANOVM_AUDIT_LOG").ok();
        std::env::remove_var("NANOVM_AUDIT_LOG");
        let log = AuditLog::from_env();
        assert!(log.is_disabled());
        if let Some(v) = prev {
            std::env::set_var("NANOVM_AUDIT_LOG", v);
        }
    }

    #[test]
    fn from_env_open_failure_disables_gracefully() {
        let prev = std::env::var("NANOVM_AUDIT_LOG").ok();
        // Path-component-as-directory hack so open() will fail without us
        // creating the path: a relative path under /proc that can't be
        // created as a regular file.
        std::env::set_var(
            "NANOVM_AUDIT_LOG",
            "/proc/this/path/cannot/possibly/exist/audit.jsonl",
        );
        let log = AuditLog::from_env();
        assert!(log.is_disabled());
        std::env::remove_var("NANOVM_AUDIT_LOG");
        if let Some(v) = prev {
            std::env::set_var("NANOVM_AUDIT_LOG", v);
        }
    }

    #[test]
    fn is_mutating_classifies_methods() {
        assert!(is_mutating(&Method::POST));
        assert!(is_mutating(&Method::PUT));
        assert!(is_mutating(&Method::PATCH));
        assert!(is_mutating(&Method::DELETE));
        assert!(!is_mutating(&Method::GET));
        assert!(!is_mutating(&Method::HEAD));
        assert!(!is_mutating(&Method::OPTIONS));
    }
}
