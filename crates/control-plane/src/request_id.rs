//! `X-Request-Id` correlation middleware.
//!
//! Every request that flows through the control plane gets an id that
//! round-trips on the response — caller-supplied when the inbound header
//! validates, freshly minted otherwise. The id is plumbed into:
//!
//! 1. The response's `X-Request-Id` header, so the caller can paste it
//!    back when filing an issue.
//! 2. The request `Extensions` as a [`RequestId`], so handlers and
//!    downstream middleware (e.g. the audit appender) can read it.
//! 3. A `tracing::info_span!` field, so structured logs from the handler
//!    inherit the correlation tag and can be pivoted on.
//!
//! ## Inbound validation
//!
//! Bad header bytes never reach the response. The middleware accepts an
//! id of up to 128 characters drawn from `[A-Za-z0-9._-]`; anything else
//! is silently replaced with a freshly minted id. We never echo
//! attacker-controlled bytes back; smuggling a CRLF into the response
//! header is the kind of bug that gets cached in a CDN and bites a year
//! later.
//!
//! ## ID format
//!
//! `nanovm-<16-hex-nanos>-<8-hex-counter>` — ~30 chars,
//! lexicographically sortable, no `uuid` or `rand` dep. Uniqueness
//! within a process is guaranteed by the atomic counter; cross-process
//! collisions are vanishingly unlikely thanks to the nanosecond prefix.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{extract::Request, http::HeaderValue, middleware::Next, response::Response};
use tracing::Instrument;

/// Maximum length of an incoming `X-Request-Id` header we accept.
const MAX_INCOMING_LEN: usize = 128;

/// Request-id attached to every request's `Extensions`. Cheap to clone.
///
/// Handlers can pull it with `req.extensions().get::<RequestId>()` or by
/// declaring an `Extension<RequestId>` extractor parameter.
#[derive(Clone, Debug)]
pub struct RequestId(pub String);

impl RequestId {
    /// The raw id string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// axum middleware: install [`RequestId`] in request extensions, echo it
/// on the response, and wrap the rest of the chain in a `tracing` span
/// keyed on the id.
///
/// Install this as the **outermost** layer so every route — including
/// `/healthz`, `/metrics`, and `/openapi.json` — receives an id.
pub async fn with_request_id(mut req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|h| h.to_str().ok())
        .filter(|s| is_valid_incoming(s))
        .map(|s| s.to_owned())
        .unwrap_or_else(mint);

    let request_id = RequestId(id.clone());
    req.extensions_mut().insert(request_id);

    let span = tracing::info_span!("request", request_id = %id);
    let mut response = next.run(req).instrument(span).await;

    // We just minted or validated `id` against the strict charset, so the
    // HeaderValue::from_str never fails. Defensive `unwrap_or_else`
    // anyway: emit a sanitized id rather than panicking if the assumption
    // ever breaks.
    let header = HeaderValue::from_str(&id).unwrap_or_else(|_| HeaderValue::from_static("nanovm"));
    response.headers_mut().insert("x-request-id", header);
    response
}

/// `true` if every char in `s` is in `[A-Za-z0-9._-]` and the length is
/// `1..=MAX_INCOMING_LEN`. Empty IDs are rejected so we don't echo
/// `X-Request-Id: ` and confuse downstream tooling.
fn is_valid_incoming(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_INCOMING_LEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Mint a fresh request id. Uniqueness within this process is guaranteed
/// by the atomic counter; the nanosecond prefix makes cross-process
/// collisions vanishingly unlikely.
fn mint() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("nanovm-{nanos:016x}-{seq:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_is_unique_within_process() {
        let mut ids = std::collections::HashSet::new();
        for _ in 0..10_000 {
            ids.insert(mint());
        }
        assert_eq!(ids.len(), 10_000, "mint() must produce unique ids");
    }

    #[test]
    fn mint_format_is_stable() {
        let id = mint();
        assert!(id.starts_with("nanovm-"));
        assert_eq!(id.len(), 7 + 16 + 1 + 8, "got {id}");
        assert!(is_valid_incoming(&id));
    }

    #[test]
    fn valid_incoming_accepts_safe_chars() {
        assert!(is_valid_incoming("nanovm-abc-123"));
        assert!(is_valid_incoming("abc.def_ghi-jkl"));
        assert!(is_valid_incoming("abcdef0123456789"));
        assert!(is_valid_incoming("a"));
    }

    #[test]
    fn valid_incoming_rejects_empty_and_overlong() {
        assert!(!is_valid_incoming(""));
        let long = "a".repeat(MAX_INCOMING_LEN + 1);
        assert!(!is_valid_incoming(&long));
    }

    #[test]
    fn valid_incoming_rejects_unsafe_chars() {
        assert!(!is_valid_incoming("abc def")); // space
        assert!(!is_valid_incoming("abc\ndef")); // newline
        assert!(!is_valid_incoming("abc/def")); // slash
        assert!(!is_valid_incoming("abc;def")); // semicolon
        assert!(!is_valid_incoming("abc:def")); // colon
        assert!(!is_valid_incoming("abc<def")); // angle bracket
        assert!(!is_valid_incoming("abc\"def")); // quote
        assert!(!is_valid_incoming("héllo")); // non-ASCII
    }
}
