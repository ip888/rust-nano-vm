//! Bearer-token authentication and per-org scoping for the control plane.
//!
//! Every accepted token belongs to exactly one org. On a successful
//! `require_token` pass, the middleware injects the calling `OrgId` into the
//! request's extensions; handlers read it to scope reads (list filters) and
//! writes (ownership inserts + cross-org 403s on subsequent ops).
//!
//! ## Env format
//!
//! `NANOVM_API_TOKENS` is a comma-separated list. Each entry is either:
//!
//! - `org_id:token`  → the token belongs to that org
//! - `token`         → the token belongs to the special org `"default"`
//!
//! The legacy comma-only shape (`tok1,tok2`) keeps working: every legacy
//! token lands in the `default` org. That preserves single-tenant
//! deployments byte-for-byte; multi-tenant rollouts re-encode the env to
//! `tenantA:tok1,tenantB:tok2`.
//!
//! ## What this module enforces
//!
//! - Authentication: token must match.
//! - Org binding: token → org identity is injected into extensions.
//!
//! What this module DOES NOT enforce (lives in `routes.rs`):
//!
//! - Resource ownership (which VMs / snapshots a given org may touch).
//! - List filtering.
//!
//! Keeping enforcement at the handler layer lets us add per-resource rules
//! (e.g. "tokens with `read_only` scope can `GET` but not `POST`") without
//! recompiling the middleware.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Extension, Request},
    middleware::Next,
    response::Response,
};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;

/// Default org assigned to tokens whose env entry has no `org_id:` prefix.
/// Single-tenant deployments stay byte-compatible with the previous shape.
pub const DEFAULT_ORG: &str = "default";

/// Owner identity for a token, a VM, or a snapshot. Cheap to clone
/// (immutable string). Equality is byte-exact and case-sensitive.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct OrgId(pub String);

impl OrgId {
    /// Build an OrgId from any string-like value.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The default org's id. Returned when a legacy token has no prefix.
    pub fn default_org() -> Self {
        Self(DEFAULT_ORG.to_owned())
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for OrgId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Set of accepted bearer tokens. Cheap to clone via `Arc`. Each token
/// maps to its owning org; cross-org access is rejected at the handler
/// layer (this struct just answers "who is this token?").
#[derive(Clone, Debug, Default)]
pub struct ApiTokens {
    tokens: HashMap<String, OrgId>,
}

impl ApiTokens {
    /// Construct from any iterable of token strings. Every token is
    /// assigned to the [`OrgId::default_org()`] — single-tenant
    /// shape. Use [`with_orgs`](Self::with_orgs) for multi-tenant
    /// wiring. Empty / whitespace-only tokens are dropped.
    pub fn new<I, S>(iter: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let default = OrgId::default_org();
        let tokens = iter
            .into_iter()
            .map(Into::into)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .map(|s| (s, default.clone()))
            .collect();
        Self { tokens }
    }

    /// Construct from `(token, org)` pairs. Use for multi-tenant
    /// wiring where each token is explicitly bound to an org.
    pub fn with_orgs<I, S>(iter: I) -> Self
    where
        I: IntoIterator<Item = (S, OrgId)>,
        S: Into<String>,
    {
        let tokens = iter
            .into_iter()
            .map(|(t, org)| (t.into().trim().to_owned(), org))
            .filter(|(t, _)| !t.is_empty())
            .collect();
        Self { tokens }
    }

    /// Build from `NANOVM_API_TOKENS`. Empty / unset → "auth disabled".
    pub fn from_env() -> Self {
        let raw = std::env::var("NANOVM_API_TOKENS").unwrap_or_default();
        Self::from_csv(&raw)
    }

    /// Parse a comma-separated list of `org_id:token` entries, with a
    /// bare `token` entry treated as `default:token`. Whitespace around
    /// each entry is trimmed; empty entries are dropped.
    pub fn from_csv(s: &str) -> Self {
        let mut out = HashMap::new();
        for entry in s.split(',') {
            let trimmed = entry.trim();
            if trimmed.is_empty() {
                continue;
            }
            let (org, token) = match trimmed.split_once(':') {
                Some((o, t)) => (OrgId::new(o.trim()), t.trim().to_owned()),
                None => (OrgId::default_org(), trimmed.to_owned()),
            };
            if token.is_empty() {
                continue;
            }
            // Later duplicates overwrite earlier ones; the env shape is
            // operator-controlled, so we keep the last wins rule explicit.
            out.insert(token, org);
        }
        Self { tokens: out }
    }

    /// `true` when no tokens are configured — middleware will allow all
    /// requests through. (Auth-disabled mode for local dev.)
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Number of accepted tokens (across all orgs).
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Whether `presented` matches any configured token, regardless of
    /// org. Kept for back-compat with call sites that don't yet care
    /// about the org; new code prefers [`org_for`](Self::org_for).
    pub fn accepts(&self, presented: &str) -> bool {
        self.tokens.contains_key(presented)
    }

    /// Returns the org `presented` belongs to, or `None` if the token
    /// isn't in the set.
    pub fn org_for(&self, presented: &str) -> Option<OrgId> {
        self.tokens.get(presented).cloned()
    }
}

/// axum middleware: require `Authorization: Bearer <token>` matching the
/// installed [`ApiTokens`] extension. On success, inserts the calling
/// [`OrgId`] into request extensions so downstream handlers can read it.
///
/// In auth-disabled mode (empty token set) the middleware injects the
/// `default` org so single-tenant local-dev deployments keep working
/// without any changes to handler code.
pub async fn require_token(
    tokens: Option<Extension<Arc<ApiTokens>>>,
    mut req: Request,
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
        // Auth disabled: anonymous traffic is treated as the default
        // org. Useful for local dev; production deployments must set
        // NANOVM_API_TOKENS so the middleware actually checks anything.
        req.extensions_mut().insert(OrgId::default_org());
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
    let Some(org) = tokens.org_for(bearer) else {
        return Err(ApiError::Unauthorized("invalid api token".into()));
    };
    req.extensions_mut().insert(org);
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_csv_legacy_no_colon_lands_in_default_org() {
        let t = ApiTokens::from_csv("alpha,beta,gamma");
        assert_eq!(t.len(), 3);
        assert_eq!(t.org_for("alpha"), Some(OrgId::default_org()));
        assert_eq!(t.org_for("beta"), Some(OrgId::default_org()));
        assert_eq!(t.org_for("gamma"), Some(OrgId::default_org()));
    }

    #[test]
    fn from_csv_with_org_prefix_assigns_org() {
        let t = ApiTokens::from_csv("acme:tok1, acme:tok2 ,globex:tok3");
        assert_eq!(t.len(), 3);
        assert_eq!(t.org_for("tok1"), Some(OrgId::new("acme")));
        assert_eq!(t.org_for("tok2"), Some(OrgId::new("acme")));
        assert_eq!(t.org_for("tok3"), Some(OrgId::new("globex")));
    }

    #[test]
    fn from_csv_mixed_legacy_and_org_form() {
        let t = ApiTokens::from_csv("acme:tok1,legacy-tok,globex:tok3");
        assert_eq!(t.len(), 3);
        assert_eq!(t.org_for("tok1"), Some(OrgId::new("acme")));
        assert_eq!(t.org_for("legacy-tok"), Some(OrgId::default_org()));
        assert_eq!(t.org_for("tok3"), Some(OrgId::new("globex")));
    }

    #[test]
    fn from_csv_empty_or_whitespace_only_disables_auth() {
        assert!(ApiTokens::from_csv("").is_empty());
        assert!(ApiTokens::from_csv("   ").is_empty());
        assert!(ApiTokens::from_csv(",,, ,").is_empty());
    }

    #[test]
    fn from_csv_skips_entries_with_empty_token_after_org_prefix() {
        // `acme:` with no token — drop.
        let t = ApiTokens::from_csv("acme:,acme:tok1");
        assert_eq!(t.len(), 1);
        assert_eq!(t.org_for("tok1"), Some(OrgId::new("acme")));
    }

    #[test]
    fn org_for_returns_none_for_unknown_token() {
        let t = ApiTokens::from_csv("acme:tok1");
        assert!(t.org_for("tok2").is_none());
        assert!(t.org_for("").is_none());
    }

    #[test]
    fn accepts_back_compat_returns_true_when_token_known() {
        let t = ApiTokens::from_csv("acme:tok1,legacy");
        assert!(t.accepts("tok1"));
        assert!(t.accepts("legacy"));
        assert!(!t.accepts("nope"));
    }

    #[test]
    fn org_id_default_is_stable() {
        assert_eq!(OrgId::default_org().as_str(), "default");
    }
}
