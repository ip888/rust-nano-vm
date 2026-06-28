//! `/v1/keys` — self-serve runtime API key management.
//!
//! Endpoints:
//!
//! | Method | Path              | Action                                                          |
//! |--------|-------------------|-----------------------------------------------------------------|
//! | POST   | `/v1/keys`        | Issue a new bearer token for the caller's org. Token shown once.|
//! | GET    | `/v1/keys`        | List the caller's runtime tokens (no plaintext).                |
//! | DELETE | `/v1/keys/:id`    | Revoke a runtime token by its public id.                        |
//!
//! Env-loaded tokens are not addressable here — they're operator-managed
//! out of band via `NANOVM_API_TOKENS`. Runtime tokens are kept in
//! memory and (when `NANOVM_TOKEN_STORE_PATH` is set) atomically
//! snapshotted to a JSON file after every `issue` / `revoke`, so they
//! survive process restarts. Multi-replica deployments still need a
//! shared store (the JSON file is per-pod); the persistence shim is
//! enough for the single-replica case the Helm chart defaults to.

use std::sync::Arc;

use axum::{
    extract::{rejection::PathRejection, Extension, Path},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::auth::{ApiTokens, OrgId, RuntimeTokenInfo, TokenId};
use crate::error::ApiError;

/// Response body for `POST /v1/keys`. The `token` field is the only
/// time the plaintext bearer is exposed; the caller MUST persist it
/// client-side (the API has no recovery path).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct IssueKeyResponse {
    /// The new bearer token. Use as `Authorization: Bearer <token>`.
    pub token: String,
    /// Public id of the new token. Use to revoke / look up later.
    pub id: String,
    /// Org the token belongs to (echoed for client convenience).
    pub org: String,
    /// Wall-clock issue time (RFC 3339).
    pub created_at: String,
}

/// Response body for `GET /v1/keys`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ListKeysResponse {
    pub keys: Vec<KeyEntry>,
}

/// One row in [`ListKeysResponse`]. Never includes the plaintext token.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct KeyEntry {
    pub id: String,
    pub org: String,
    pub created_at: String,
}

impl From<RuntimeTokenInfo> for KeyEntry {
    fn from(i: RuntimeTokenInfo) -> Self {
        Self {
            id: i.id.to_string(),
            org: i.org.to_string(),
            created_at: i.created_at,
        }
    }
}

/// `POST /v1/keys` — mint a new bearer token for the caller's org.
pub(crate) async fn issue_key(
    Extension(tokens): Extension<Arc<ApiTokens>>,
    Extension(org): Extension<OrgId>,
) -> Result<(StatusCode, Json<IssueKeyResponse>), ApiError> {
    let issued = tokens.issue(org);
    Ok((
        StatusCode::CREATED,
        Json(IssueKeyResponse {
            token: issued.token,
            id: issued.id.to_string(),
            org: issued.org.to_string(),
            created_at: issued.created_at,
        }),
    ))
}

/// `GET /v1/keys` — list the caller org's runtime tokens (no plaintext).
/// Sorted by `id` for a stable wire shape.
pub(crate) async fn list_keys(
    Extension(tokens): Extension<Arc<ApiTokens>>,
    Extension(org): Extension<OrgId>,
) -> Json<ListKeysResponse> {
    let mut keys: Vec<KeyEntry> = tokens
        .list_runtime(&org)
        .into_iter()
        .map(KeyEntry::from)
        .collect();
    keys.sort_by(|a, b| a.id.cmp(&b.id));
    Json(ListKeysResponse { keys })
}

/// `DELETE /v1/keys/:id` — revoke a runtime token. Returns 404 if the id
/// is unknown or belongs to a different org (we don't distinguish, to
/// avoid leaking other orgs' id space). Env tokens are never revokable
/// here; the operator manages them out of band.
pub(crate) async fn revoke_key(
    Extension(tokens): Extension<Arc<ApiTokens>>,
    Extension(org): Extension<OrgId>,
    id: Result<Path<String>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Path(id) = id?;
    let id = TokenId(id);
    if tokens.revoke(&id, &org) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound {
            code: "unknown_key",
            message: format!("key {id} not found"),
        })
    }
}
