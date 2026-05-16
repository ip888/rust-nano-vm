//! HTTP error envelope.
//!
//! Every failure mode — including extractor rejections (malformed JSON
//! bodies, non-numeric path ids) and errors surfaced by the hypervisor —
//! renders with the same JSON shape:
//!
//! ```json
//! { "error": { "code": "unknown_vm", "message": "unknown vm id: vm-..." } }
//! ```
//!
//! `code` is stable and safe to match on; `message` is a human-readable
//! detail that may change between releases.

use axum::{
    extract::rejection::{JsonRejection, PathRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use vm_core::VmError;

/// Error returned from a handler.
#[derive(Debug)]
pub(crate) enum ApiError {
    /// Failure surfaced by the hypervisor backend.
    Vm(VmError),
    /// The request body failed JSON decoding (malformed JSON, wrong shape,
    /// wrong content-type).
    BadJson(JsonRejection),
    /// A URL path segment failed to parse into the expected type (e.g. a
    /// non-numeric `:id`).
    BadPath(PathRejection),
    /// The request lacked a valid bearer token. The `String` is the reason
    /// (surfaced to the client in `message`).
    Unauthorized(String),
    /// The server is misconfigured in a way the client can't fix (e.g. a
    /// required `axum::Extension` was not installed at startup). Surfaced as
    /// 500 with code "internal" — the message is intended for the operator
    /// reading server logs, not for end-user diagnosis.
    Internal(&'static str),
    /// Generic "client error" envelope a handler can synthesize when it
    /// parses the request body itself (bypassing axum's JSON extractor).
    /// Renders as 400 with `code: "bad_request"`.
    Bad(String),
    /// Per-token rate limit exceeded. Renders as 429 with
    /// `code: "too_many_requests"` and a `Retry-After` header carrying
    /// the seconds-until-refill hint.
    TooManyRequests {
        /// Estimated time the client should wait before retrying, in
        /// fractional seconds.
        retry_after_secs: f64,
    },
}

impl From<VmError> for ApiError {
    fn from(e: VmError) -> Self {
        Self::Vm(e)
    }
}

impl From<JsonRejection> for ApiError {
    fn from(e: JsonRejection) -> Self {
        Self::BadJson(e)
    }
}

impl From<PathRejection> for ApiError {
    fn from(e: PathRejection) -> Self {
        Self::BadPath(e)
    }
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            ApiError::Vm(e) => {
                let msg = e.to_string();
                let (status, code) = match e {
                    VmError::UnknownVm(_) => (StatusCode::NOT_FOUND, "unknown_vm"),
                    VmError::UnknownSnapshot(_) => (StatusCode::NOT_FOUND, "unknown_snapshot"),
                    VmError::InvalidTransition { .. } => {
                        (StatusCode::CONFLICT, "invalid_transition")
                    }
                    VmError::Unsupported(_) => (StatusCode::NOT_IMPLEMENTED, "unsupported"),
                    VmError::Backend(_) => (StatusCode::INTERNAL_SERVER_ERROR, "backend"),
                    // `VmError` is #[non_exhaustive]; any future variant falls
                    // back to 500 until we add a dedicated mapping.
                    _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
                };
                (status, code, msg)
            }
            ApiError::BadJson(rej) => (rej.status(), "bad_request", rej.body_text()),
            ApiError::BadPath(rej) => (rej.status(), "bad_request", rej.body_text()),
            ApiError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, "unauthorized", msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, "internal", msg.into()),
            ApiError::Bad(msg) => (StatusCode::BAD_REQUEST, "bad_request", msg),
            ApiError::TooManyRequests { retry_after_secs } => {
                // Round up so a 0.1s wait becomes "1" not "0" in the
                // Retry-After header (which is a non-negative integer
                // count of seconds per RFC 9110).
                let secs = retry_after_secs.ceil().max(1.0) as u64;
                let body = Json(ErrorEnvelope {
                    error: ErrorBody {
                        code: "too_many_requests",
                        message: format!("rate limit exceeded; retry after {retry_after_secs:.3}s"),
                    },
                });
                let mut resp = (StatusCode::TOO_MANY_REQUESTS, body).into_response();
                resp.headers_mut().insert(
                    axum::http::header::RETRY_AFTER,
                    axum::http::HeaderValue::from(secs),
                );
                return resp;
            }
        };
        let body = Json(ErrorEnvelope {
            error: ErrorBody { code, message },
        });
        (status, body).into_response()
    }
}
