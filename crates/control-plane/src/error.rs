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
        };
        let body = Json(ErrorEnvelope {
            error: ErrorBody { code, message },
        });
        (status, body).into_response()
    }
}
