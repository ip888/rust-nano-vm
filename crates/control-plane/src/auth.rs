//! Bearer-token authentication for the control plane.
//!
//! On startup the binary reads `NANOVM_API_TOKENS` (comma-separated) into an
//! [`ApiTokens`] set and installs it as an [`axum::Extension`]. The
//! [`require_token`] middleware then guards every `/v1/*` route — `/healthz`
//! is intentionally exempt so external liveness probes don't carry secrets.
//!
//! If `NANOVM_API_TOKENS` is unset (or expands to an empty list) the
//! middleware short-circuits to "auth disabled". The binary logs a `WARN`
//! line in that case so a misconfiguration in production is loud rather
//! than silent.
//!
//! Tokens are compared with `eq` against the set; the set lookup is
//! `O(1)` and timing-independent for legitimately-formatted tokens. (We do
//! not promise constant-time comparison against an attacker who knows the
//! token format precisely; for that, plug a `subtle`-based comparator. Out
//! of scope for v1.)

use std::collections::HashSet;
use std::sync::Arc;

use axum::{
    extract::{Extension, Request},
    middleware::Next,
    response::Response,
};

use crate::error::ApiError;

/// Set of accepted bearer tokens. Cheap to clone via `Arc`.
#[derive(Clone, Debug, Default)]
pub struct ApiTokens {
    tokens: HashSet<String>,
}

impl ApiTokens {
    /// Construct from any iterable of token strings. Empty / whitespace-only
    /// entries are dropped.
    pub fn new<I, S>(iter: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let tokens = iter
            .into_iter()
            .map(Into::into)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        Self { tokens }
    }

    /// Build from the `NANOVM_API_TOKENS` environment variable, parsed as a
    /// comma-separated token list. If the variable is unset or expands to
    /// only empty/whitespace entries, returns an empty set (auth disabled).
    pub fn from_env() -> Self {
        let raw = std::env::var("NANOVM_API_TOKENS").unwrap_or_default();
        Self::from_csv(&raw)
    }

    /// Parse a comma-separated list of tokens. Whitespace around each entry
    /// is trimmed.
    pub fn from_csv(s: &str) -> Self {
        Self::new(s.split(','))
    }

    /// `true` when no tokens are configured — middleware will allow all
    /// requests through.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Number of accepted tokens.
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Whether `presented` matches any configured token.
    pub fn accepts(&self, presented: &str) -> bool {
        self.tokens.contains(presented)
    }
}

/// axum middleware: require `Authorization: Bearer <token>` matching the
/// installed [`ApiTokens`] extension. Apply via `route_layer` so
/// route-not-found returns 404 even when the token is missing (avoids
/// turning the API surface into a token oracle).
///
/// When [`ApiTokens::is_empty`] is true the middleware short-circuits to
/// allow the request — auth is "off". The binary logs a warning at startup
/// in that mode so the operator notices.
///
/// If the [`ApiTokens`] extension is not installed at all, this returns a
/// structured [`ApiError::Internal`] (500 `"internal"`) rather than letting
/// axum short-circuit with its plain-text `ExtensionRejection`. That keeps
/// the error envelope contract intact even when a library consumer forgets
/// to call `.layer(Extension(...))`.
pub async fn require_token(
    tokens: Option<Extension<Arc<ApiTokens>>>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let Some(Extension(tokens)) = tokens else {
        return Err(ApiError::Internal(
            "auth middleware installed but ApiTokens extension is missing \
             — call `.layer(Extension(Arc::new(ApiTokens::from_env())))` \
             on the router before serving",
        ));
    };
    if tokens.is_empty() {
        return Ok(next.run(req).await);
    }
    let bearer = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or_else(|| {
            ApiError::Unauthorized("missing or malformed authorization header".into())
        })?;
    if !tokens.accepts(bearer) {
        return Err(ApiError::Unauthorized("invalid api token".into()));
    }
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_csv_drops_empty_and_whitespace() {
        let t = ApiTokens::from_csv("alpha, beta ,, ,gamma");
        assert_eq!(t.len(), 3);
        assert!(t.accepts("alpha"));
        assert!(t.accepts("beta"));
        assert!(t.accepts("gamma"));
        assert!(!t.accepts(""));
    }

    #[test]
    fn empty_set_disables_auth() {
        assert!(ApiTokens::from_csv("").is_empty());
        assert!(ApiTokens::from_csv("   ,  ,").is_empty());
        assert!(ApiTokens::default().is_empty());
    }

    #[test]
    fn rejects_token_not_in_set() {
        let t = ApiTokens::new(["secret-1", "secret-2"]);
        assert!(t.accepts("secret-1"));
        assert!(!t.accepts("secret-3"));
        assert!(!t.accepts("Secret-1"), "case-sensitive");
    }
}
