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
    /// The caller authenticated successfully but is forbidden from
    /// touching the addressed resource. Used for cross-org access on
    /// VMs / snapshots / etc. Renders as 403 with a caller-supplied
    /// stable code.
    Forbidden {
        /// Stable machine-readable code (e.g. `"cross_org"`).
        code: &'static str,
        /// Human-readable detail surfaced to the client.
        message: String,
    },
    /// The server is misconfigured in a way the client can't fix (e.g. a
    /// required `axum::Extension` was not installed at startup). Surfaced as
    /// 500 with code "internal" — the message is intended for the operator
    /// reading server logs, not for end-user diagnosis.
    Internal(&'static str),
    /// Generic "client error" envelope a handler can synthesize when it
    /// parses the request body itself (bypassing axum's JSON extractor).
    /// Renders as 400 with `code: "bad_request"`.
    Bad(String),
    /// Per-token quota exceeded. Renders as 429 with the `Retry-After`
    /// header set to `retry_after_secs` and the structured envelope
    /// `{ error: { code, message } }`.
    TooManyRequests {
        /// Stable machine-readable code (e.g. `"fork_quota_exceeded"`).
        code: &'static str,
        /// Human-readable detail surfaced to the client.
        message: String,
        /// Seconds the client should wait before retrying.
        retry_after_secs: u64,
    },
    /// Endpoint or feature isn't supported on this deployment (no
    /// snapshot store configured, backend doesn't know how to do X,
    /// etc.). Renders as 501 with a caller-supplied stable code.
    Unsupported {
        /// Stable machine-readable code (e.g. `"storage_unsupported"`).
        code: &'static str,
        /// Human-readable detail surfaced to the client.
        message: String,
    },
    /// Resource not found, with a caller-supplied stable code so
    /// callers can distinguish (e.g. `"snapshot_not_in_store"` vs.
    /// `"unknown_snapshot"`). Renders as 404.
    NotFound {
        /// Stable machine-readable code.
        code: &'static str,
        /// Human-readable detail surfaced to the client.
        message: String,
    },
    /// Internal failure with a dynamic message (failed background
    /// task, IO error during durable-store interaction, …). Renders
    /// as 500 with code `"internal"`. The static-message variant
    /// [`ApiError::Internal`] stays for operator-facing constants.
    InternalDyn(String),
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
        // 429 needs a Retry-After header in addition to the JSON envelope, so
        // build it directly rather than going through the (status, body) path.
        if let ApiError::TooManyRequests {
            code,
            message,
            retry_after_secs,
        } = self
        {
            let body = Json(ErrorEnvelope {
                error: ErrorBody { code, message },
            });
            let mut resp = (StatusCode::TOO_MANY_REQUESTS, body).into_response();
            if let Ok(v) = retry_after_secs.to_string().parse() {
                resp.headers_mut().insert("retry-after", v);
            }
            return resp;
        }
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
            ApiError::Forbidden { code, message } => (StatusCode::FORBIDDEN, code, message),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, "internal", msg.into()),
            ApiError::InternalDyn(msg) => (StatusCode::INTERNAL_SERVER_ERROR, "internal", msg),
            ApiError::Bad(msg) => (StatusCode::BAD_REQUEST, "bad_request", msg),
            ApiError::Unsupported { code, message } => (StatusCode::NOT_IMPLEMENTED, code, message),
            ApiError::NotFound { code, message } => (StatusCode::NOT_FOUND, code, message),
            ApiError::TooManyRequests { .. } => {
                unreachable!("TooManyRequests handled above with Retry-After header")
            }
        };
        let body = Json(ErrorEnvelope {
            error: ErrorBody { code, message },
        });
        (status, body).into_response()
    }
}
