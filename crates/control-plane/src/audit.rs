//! JSONL append-only audit log for mutating API calls.
//!
//! Captures every `POST`/`PUT`/`PATCH`/`DELETE` against `/v1/*`
//! (and only those — `GET` is non-mutating and would just spam the
//! log) as a single JSON object per line:
//!
//! ```json
//! {"ts":"2026-05-17T00:00:00.123Z","method":"POST","path":"/v1/vms",
//!  "status":201,"token":"tok-abcd-12"}
//! ```
//!
//! Closes tracked gap **G4** from `docs/threat-model.md`.
//!
//! Path is taken from `NANOVM_AUDIT_LOG` at startup. Unset →
//! middleware short-circuits and no file is opened. The binary
//! warns at startup if you boot with auth enabled but the audit
//! log disabled (the combination operators usually want is "both
//! on").
//!
//! **Failure mode**: writes are best-effort. If the disk fills or
//! the file is unwritable, we log an `ERROR` once and the request
//! still completes successfully. We do **not** fail the request on
//! a log-write failure — losing one audit line is preferable to
//! losing every request that comes after.
//!
//! **Bearer-token handling**: the raw token is **never** written
//! to the log. We emit a non-cryptographic fingerprint
//! (`tok-{first4}-{len}`) so an operator chasing a suspected leak
//! can distinguish "all of these were the same token" from "many
//! distinct tokens", without the log itself being a credential
//! store.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Extension, OriginalUri, Request},
    middleware::Next,
    response::Response,
};
use serde::Serialize;

/// Handle to the open audit-log file. Cheap to clone via `Arc`.
#[derive(Debug)]
pub struct AuditLog {
    /// `None` when audit is disabled (no `NANOVM_AUDIT_LOG` set).
    /// `Some(file)` otherwise — wrapped in a `Mutex` because the
    /// audit middleware appends from many concurrent tasks.
    sink: Mutex<Option<std::fs::File>>,
    /// Original path string, kept for the startup log line.
    path: Option<String>,
}

impl AuditLog {
    /// Build a disabled audit log (passes through every request).
    pub fn disabled() -> Self {
        Self {
            sink: Mutex::new(None),
            path: None,
        }
    }

    /// Open `path` in append mode, creating it if it doesn't exist.
    /// Returns the disabled variant + an error if the open fails;
    /// callers should surface the error to the operator but **not**
    /// fail startup — running without an audit log is degraded but
    /// not broken.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let p = path.as_ref();
        let file = OpenOptions::new().create(true).append(true).open(p)?;
        Ok(Self {
            sink: Mutex::new(Some(file)),
            path: Some(p.display().to_string()),
        })
    }

    /// Construct from `NANOVM_AUDIT_LOG`. Unset → disabled.
    /// Open failure → disabled + the `io::Error` so the binary can
    /// log it on the way up.
    pub fn from_env() -> Result<Self, std::io::Error> {
        match std::env::var("NANOVM_AUDIT_LOG") {
            Ok(p) if !p.is_empty() => Self::open(p),
            _ => Ok(Self::disabled()),
        }
    }

    /// `true` when no file is open (passes every request through).
    pub fn is_disabled(&self) -> bool {
        self.sink.lock().expect("audit sink poisoned").is_none()
    }

    /// Path the audit log writes to. `None` when disabled.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Append one record. Best-effort: a write failure is logged
    /// at `ERROR` but doesn't propagate.
    fn append(&self, record: &Record<'_>) {
        let mut guard = self.sink.lock().expect("audit sink poisoned");
        let Some(file) = guard.as_mut() else { return };
        let mut line = match serde_json::to_string(record) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(err = %e, "audit: serialize failed; dropping record");
                return;
            }
        };
        line.push('\n');
        if let Err(e) = file.write_all(line.as_bytes()) {
            tracing::error!(err = %e, "audit: write failed; dropping record");
        }
    }
}

/// One audit record. Serialized as a JSON object per line.
#[derive(Debug, Serialize)]
struct Record<'a> {
    /// RFC 3339 UTC timestamp, millisecond precision.
    ts: String,
    /// HTTP method (`POST`/`PUT`/`PATCH`/`DELETE`).
    method: &'a str,
    /// Request path including any `?` query string.
    path: &'a str,
    /// Final HTTP status code.
    status: u16,
    /// Bearer-token fingerprint — `tok-{first4}-{len}` — or
    /// `"-"` when auth is disabled or no bearer was presented.
    token: String,
}

/// Non-cryptographic bearer fingerprint. Stable for a given token
/// string, distinguishes "same token" from "different token", but
/// reveals nothing useful about the secret (first 4 chars + length
/// only). Returns `"-"` for empty / missing.
fn fingerprint(bearer: Option<&str>) -> String {
    let Some(b) = bearer else {
        return "-".to_string();
    };
    if b.len() < 4 {
        // Shouldn't happen — auth would have rejected — but degrade
        // safely if it does.
        return format!("tok-(short:{})", b.len());
    }
    format!("tok-{}-{}", &b[..4], b.len())
}

/// Axum middleware. Install on `/v1/*` after `require_token` so
/// the bearer header is present when we log it. Skips `GET` (and
/// `HEAD`/`OPTIONS`) since the threat model only requires audit of
/// *mutating* calls.
///
/// Tolerant of a missing `AuditLog` extension: library consumers
/// that don't install one pay zero cost. Tolerant of disabled log
/// (no `NANOVM_AUDIT_LOG` env): the middleware runs but never
/// touches a file.
pub async fn audit_mutating(
    audit: Option<Extension<std::sync::Arc<AuditLog>>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(Extension(audit)) = audit else {
        return next.run(req).await;
    };
    if audit.is_disabled() {
        return next.run(req).await;
    }
    let method = req.method().clone();
    if !matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE") {
        return next.run(req).await;
    }
    // Prefer `OriginalUri` (axum extension) which preserves the pre-
    // `nest` path — `req.uri()` inside a `route_layer` on a nested
    // sub-router has been stripped of the `/v1` prefix, which would
    // make audit lines ambiguous. Fall back to `req.uri()` for
    // tests / library consumers that wire the audit middleware
    // somewhere other than under `nest("/v1", …)`.
    let path = req
        .extensions()
        .get::<OriginalUri>()
        .map(|o| {
            o.0.path_and_query()
                .map(|p| p.as_str().to_owned())
                .unwrap_or_else(|| o.0.path().to_owned())
        })
        .unwrap_or_else(|| {
            req.uri()
                .path_and_query()
                .map(|p| p.as_str().to_owned())
                .unwrap_or_else(|| req.uri().path().to_owned())
        });
    let token = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    let token_fp = fingerprint(token);

    let response = next.run(req).await;

    audit.append(&Record {
        ts: rfc3339_now(),
        method: method.as_str(),
        path: &path,
        status: response.status().as_u16(),
        token: token_fp,
    });
    response
}

/// Hand-rolled RFC 3339 UTC string with millisecond precision —
/// avoids pulling in `chrono` for what is ultimately a `format!`.
fn rfc3339_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = now.as_secs();
    let millis = now.subsec_millis();
    let (year, month, day, hour, min, sec) = epoch_to_ymdhms(total_secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{millis:03}Z")
}

/// Civil-from-days algorithm (Howard Hinnant). Converts unix
/// seconds → (Y, M, D, h, m, s). Good for all dates `[1970-01-01,
/// year 5000]`; we don't ship a leap-second table because audit
/// timestamps don't need it.
fn epoch_to_ymdhms(secs: u64) -> (i64, u8, u8, u8, u8, u8) {
    let days = (secs / 86_400) as i64;
    let secs_of_day = (secs % 86_400) as u32;
    let hour = (secs_of_day / 3600) as u8;
    let min = ((secs_of_day % 3600) / 60) as u8;
    let sec = (secs_of_day % 60) as u8;
    // Hinnant's civil_from_days, see http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day, hour, min, sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_handles_missing_and_short() {
        assert_eq!(fingerprint(None), "-");
        assert_eq!(fingerprint(Some("ab")), "tok-(short:2)");
        assert_eq!(fingerprint(Some("abcd1234")), "tok-abcd-8");
    }

    #[test]
    fn fingerprint_does_not_leak_the_secret() {
        // The whole point: an attacker reading the audit log
        // cannot reconstruct the token.
        let fp = fingerprint(Some("super-secret-bearer-token-xyz789"));
        assert!(!fp.contains("xyz789"));
        assert!(!fp.contains("secret"));
        assert_eq!(fp, "tok-supe-32");
    }

    #[test]
    fn disabled_log_short_circuits_append() {
        // append() on a disabled log must be a no-op (no panic).
        let log = AuditLog::disabled();
        log.append(&Record {
            ts: "x".into(),
            method: "POST",
            path: "/v1/vms",
            status: 200,
            token: "-".into(),
        });
        assert!(log.is_disabled());
    }

    #[test]
    fn open_then_append_then_read_back_one_line() {
        let dir = std::env::temp_dir().join("nanovm-audit-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("audit-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let log = AuditLog::open(&path).expect("open audit log");
        log.append(&Record {
            ts: "2026-05-17T00:00:00.000Z".into(),
            method: "POST",
            path: "/v1/vms",
            status: 201,
            token: "tok-abcd-12".into(),
        });
        // Drop the file handle so the OS flushes before we read.
        drop(log);

        let contents = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(contents.ends_with('\n'));
        let parsed: serde_json::Value = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(parsed["method"], "POST");
        assert_eq!(parsed["path"], "/v1/vms");
        assert_eq!(parsed["status"], 201);
        assert_eq!(parsed["token"], "tok-abcd-12");
    }

    #[test]
    fn rfc3339_now_has_expected_shape() {
        let s = rfc3339_now();
        // 2026-05-17T00:00:00.000Z = 24 chars
        assert_eq!(s.len(), 24, "unexpected len in {s}");
        assert!(s.ends_with('Z'));
        assert!(s.contains('T'));
    }

    #[test]
    fn epoch_to_ymdhms_known_anchors() {
        // Anchors derived from `date -u -d @<epoch>` so they're
        // reproducible: 1970-01-01, 2000-01-01, 2020-02-29 (leap),
        // 2021-03-01 (day after non-leap-cycle boundary).
        assert_eq!(epoch_to_ymdhms(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(epoch_to_ymdhms(946_684_800), (2000, 1, 1, 0, 0, 0));
        assert_eq!(epoch_to_ymdhms(1_582_934_400), (2020, 2, 29, 0, 0, 0));
        assert_eq!(epoch_to_ymdhms(1_614_556_800), (2021, 3, 1, 0, 0, 0));
        // Hours / minutes / seconds round-trip.
        assert_eq!(
            epoch_to_ymdhms(946_684_800 + 12 * 3600 + 34 * 60 + 56),
            (2000, 1, 1, 12, 34, 56)
        );
    }
}
