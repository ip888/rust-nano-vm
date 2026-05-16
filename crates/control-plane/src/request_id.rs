//! `X-Request-Id` correlation middleware.
//!
//! Every request gets an id that round-trips on the response. If the
//! caller provides an `X-Request-Id` header we use it verbatim (after
//! a sanity-bound check); otherwise we mint a fresh id. The id is
//! attached to the request extensions so handlers can pull it for
//! structured logs, and to a `tracing` span so trace lines for one
//! request can be correlated end-to-end.
//!
//! Closes tracked gap **G2** from `docs/threat-model.md`.
//!
//! The id format is `nanovm-{16-hex-nanos}-{8-hex-counter}` —
//! lexicographically sortable, ~30 chars, no dependency on `uuid` or
//! `rand`. Uniqueness within a process is guaranteed by the atomic
//! counter; cross-process collisions are vanishingly unlikely given
//! the nanosecond prefix.
//!
//! Sanity bound on inbound ids: max 128 ASCII chars,
//! `[A-Za-z0-9._-]` only. Anything else is replaced with a freshly
//! minted id (we do **not** echo attacker-controlled headers).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::Request,
    http::{HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};

/// Canonical header name for the correlation id, both on inbound
/// requests we honour and on outbound responses we emit.
pub const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Maximum length we'll accept from a client-supplied id before
/// falling back to a freshly minted one. 128 chars is enough for a
/// UUID-with-prefix and short enough that we won't blow up log lines.
const MAX_INBOUND_LEN: usize = 128;

/// Wrapper around the per-request id, stored in request extensions
/// so handlers can pull it via `req.extensions().get::<RequestId>()`.
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

impl RequestId {
    /// Borrow the id as a `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Counter feeding the suffix of every minted id. Wraps; combined
/// with the nanosecond prefix that is fine.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Mint a fresh, process-unique id.
fn mint() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("nanovm-{nanos:016x}-{n:08x}")
}

/// `true` iff the byte is allowed in an inbound id.
fn is_id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
}

/// Inspect an inbound header value: return the borrowed id if it
/// passes the length + charset check, else `None`.
fn validate_inbound(raw: &HeaderValue) -> Option<&str> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_INBOUND_LEN {
        return None;
    }
    if !bytes.iter().copied().all(is_id_byte) {
        return None;
    }
    // ASCII-only by construction (is_ascii_alphanumeric covers it),
    // so the str::from_utf8 is infallible.
    std::str::from_utf8(bytes).ok()
}

/// Axum middleware: install the request id in extensions, echo it on
/// the response. Install at the outermost level so every route gets
/// it (including `/healthz`).
pub async fn propagate(mut req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get(REQUEST_ID_HEADER.clone())
        .and_then(validate_inbound)
        .map(ToOwned::to_owned)
        .unwrap_or_else(mint);

    // Annotate the tracing span so log lines correlate. Use a `Span`
    // entered for the duration of the request so any handler-emitted
    // events inherit the field.
    let span = tracing::info_span!("http", request_id = %id);
    let _enter = span.enter();

    req.extensions_mut().insert(RequestId(id.clone()));
    let mut response = next.run(req).await;
    // Construction is infallible: `id` has already passed the charset
    // check (ASCII alphanumeric + . _ -), so it is a valid header value.
    if let Ok(hv) = HeaderValue::from_str(&id) {
        response.headers_mut().insert(REQUEST_ID_HEADER.clone(), hv);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_ids_are_unique_within_process() {
        let a = mint();
        let b = mint();
        assert_ne!(a, b);
        assert!(a.starts_with("nanovm-"));
    }

    #[test]
    fn validate_accepts_safe_charsets() {
        let hv = HeaderValue::from_static("abc.123_xyz-09");
        assert_eq!(validate_inbound(&hv), Some("abc.123_xyz-09"));
    }

    #[test]
    fn validate_rejects_empty() {
        let hv = HeaderValue::from_static("");
        assert_eq!(validate_inbound(&hv), None);
    }

    #[test]
    fn validate_rejects_overlong() {
        let s = "a".repeat(MAX_INBOUND_LEN + 1);
        let hv = HeaderValue::from_str(&s).unwrap();
        assert_eq!(validate_inbound(&hv), None);
    }

    #[test]
    fn validate_rejects_bad_chars() {
        // Spaces, slashes, semicolons, quotes — common header
        // injection vectors. CR/LF can't even reach validate_inbound
        // (the HTTP layer rejects them at parse time) so they aren't
        // exercised here.
        for s in ["has space", "has/slash", "has;semi", "has\"quote"] {
            let hv = HeaderValue::from_str(s).unwrap();
            assert!(
                validate_inbound(&hv).is_none(),
                "should have rejected {s:?}"
            );
        }
    }

    #[test]
    fn minted_id_is_below_header_length_bound() {
        // Sanity: our minted ids must always round-trip through
        // validate_inbound, otherwise a downstream proxy that
        // re-validates would reject them.
        let id = mint();
        let hv = HeaderValue::from_str(&id).unwrap();
        assert_eq!(validate_inbound(&hv), Some(id.as_str()));
    }
}
