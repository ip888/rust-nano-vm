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
//! ## Token sources
//!
//! Tokens come from two places:
//!
//! - **Env-loaded** at startup from `NANOVM_API_TOKENS`. Operator-managed;
//!   the API does not let them be revoked at runtime (the operator owns
//!   them and a redeploy is the right tool).
//! - **Runtime-issued** via `POST /v1/keys` — every org can self-serve a
//!   key for itself. These have a `TokenId` and can be listed / revoked
//!   via the `/v1/keys` endpoints. Runtime tokens are in-memory only for
//!   now; persistence ships in a follow-up so single-replica deployments
//!   survive restarts.
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
use std::fs::File;
use std::io::Read;
use std::sync::{Arc, RwLock};

use axum::{
    extract::{Extension, Request},
    middleware::Next,
    response::Response,
};
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::time::rfc3339_now;

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

/// Public identifier for a runtime-issued token. Safe to log and to
/// surface in API responses (does not let the holder authenticate). Used
/// as the path segment in `DELETE /v1/keys/:id`.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct TokenId(pub String);

impl TokenId {
    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TokenId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Metadata for one accepted token. We never store the plaintext on this
/// struct (it lives only as the HashMap key for the auth lookup path).
#[derive(Clone, Debug)]
struct TokenEntry {
    org: OrgId,
    source: TokenSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TokenSource {
    /// Loaded from `NANOVM_API_TOKENS`. Cannot be revoked at runtime.
    Env,
    /// Issued at runtime via `POST /v1/keys`. Listed / revoked via the
    /// `/v1/keys` API. `created_at` is an RFC 3339 wall-clock string at
    /// issue time.
    Runtime { id: TokenId, created_at: String },
}

#[derive(Clone, Debug, Default)]
struct Store {
    /// Auth lookup: token plaintext → entry. The plaintext lives here
    /// only; the management API surfaces only the `TokenId`.
    by_token: HashMap<String, TokenEntry>,
    /// Runtime token id → plaintext, so revocation (which is keyed by id)
    /// can find the right HashMap entry to drop from `by_token`.
    id_to_token: HashMap<TokenId, String>,
}

/// Set of accepted bearer tokens. Cheap to clone via `Arc`. Each token
/// maps to its owning org; cross-org access is rejected at the handler
/// layer (this struct just answers "who is this token?").
///
/// Internally guarded by an `RwLock`: the hot path (every authenticated
/// request) takes a read lock; the cold path (issue / revoke) takes a
/// write lock.
#[derive(Debug, Default)]
pub struct ApiTokens {
    store: RwLock<Store>,
}

/// Public metadata for one runtime-issued token. Returned from
/// [`ApiTokens::list_runtime`] for the `/v1/keys` listing API. Never
/// includes the plaintext token.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeTokenInfo {
    /// Public identifier — safe to log and to display.
    pub id: TokenId,
    /// Org the token belongs to.
    pub org: OrgId,
    /// Wall-clock issue time as an RFC 3339 string.
    pub created_at: String,
}

/// Outcome of [`ApiTokens::issue`] — the plaintext token (shown to the
/// caller ONCE on creation and never again) and its public id.
#[derive(Clone, Debug)]
pub struct IssuedToken {
    /// The bearer token. Hand to the caller; do not persist server-side.
    pub token: String,
    /// Public id used to revoke / list this token later.
    pub id: TokenId,
    /// Org the token belongs to.
    pub org: OrgId,
    /// Wall-clock issue time as an RFC 3339 string.
    pub created_at: String,
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
        let mut store = Store::default();
        for s in iter {
            let s = s.into().trim().to_owned();
            if s.is_empty() {
                continue;
            }
            store.by_token.insert(
                s,
                TokenEntry {
                    org: default.clone(),
                    source: TokenSource::Env,
                },
            );
        }
        Self {
            store: RwLock::new(store),
        }
    }

    /// Construct from `(token, org)` pairs. Use for multi-tenant
    /// wiring where each token is explicitly bound to an org.
    pub fn with_orgs<I, S>(iter: I) -> Self
    where
        I: IntoIterator<Item = (S, OrgId)>,
        S: Into<String>,
    {
        let mut store = Store::default();
        for (t, org) in iter {
            let t = t.into().trim().to_owned();
            if t.is_empty() {
                continue;
            }
            store.by_token.insert(
                t,
                TokenEntry {
                    org,
                    source: TokenSource::Env,
                },
            );
        }
        Self {
            store: RwLock::new(store),
        }
    }

    /// Build from `NANOVM_API_TOKENS`. Empty / unset → "auth disabled".
    pub fn from_env() -> Self {
        let raw = std::env::var("NANOVM_API_TOKENS").unwrap_or_default();
        Self::from_csv(&raw)
    }

    /// Parse a comma-separated list of `org_id:token` entries, with a
    /// bare `token` entry treated as `default:token`. Whitespace around
    /// each entry is trimmed; empty entries are dropped. An entry with
    /// an empty org id (`:tok`) is treated as the default-org form
    /// (`default:tok`) rather than the literal empty-string org — an
    /// `OrgId("")` would land in metric labels and break Grafana
    /// queries.
    pub fn from_csv(s: &str) -> Self {
        let mut store = Store::default();
        for entry in s.split(',') {
            let trimmed = entry.trim();
            if trimmed.is_empty() {
                continue;
            }
            let (org, token) = match trimmed.split_once(':') {
                Some((o, t)) => {
                    let org_str = o.trim();
                    let org = if org_str.is_empty() {
                        OrgId::default_org()
                    } else {
                        OrgId::new(org_str)
                    };
                    (org, t.trim().to_owned())
                }
                None => (OrgId::default_org(), trimmed.to_owned()),
            };
            if token.is_empty() {
                continue;
            }
            // Later duplicates overwrite earlier ones; the env shape is
            // operator-controlled, so we keep the last wins rule explicit.
            store.by_token.insert(
                token,
                TokenEntry {
                    org,
                    source: TokenSource::Env,
                },
            );
        }
        Self {
            store: RwLock::new(store),
        }
    }

    /// `true` when no tokens are configured — middleware will allow all
    /// requests through. (Auth-disabled mode for local dev.)
    pub fn is_empty(&self) -> bool {
        self.store
            .read()
            .expect("api tokens lock")
            .by_token
            .is_empty()
    }

    /// Number of accepted tokens (across all orgs).
    pub fn len(&self) -> usize {
        self.store.read().expect("api tokens lock").by_token.len()
    }

    /// Whether `presented` matches any configured token, regardless of
    /// org. Kept for back-compat with call sites that don't yet care
    /// about the org; new code prefers [`org_for`](Self::org_for).
    pub fn accepts(&self, presented: &str) -> bool {
        self.store
            .read()
            .expect("api tokens lock")
            .by_token
            .contains_key(presented)
    }

    /// Returns the org `presented` belongs to, or `None` if the token
    /// isn't in the set.
    pub fn org_for(&self, presented: &str) -> Option<OrgId> {
        self.store
            .read()
            .expect("api tokens lock")
            .by_token
            .get(presented)
            .map(|e| e.org.clone())
    }

    /// Issue a new runtime token for `org`. Returns the plaintext token
    /// (the only time it's ever exposed) plus its public id for later
    /// listing / revocation. The token is added to the in-memory accept
    /// set immediately so subsequent requests succeed.
    pub fn issue(&self, org: OrgId) -> IssuedToken {
        let token = generate_token();
        let id = TokenId(generate_token_id());
        let created_at = rfc3339_now();
        let mut store = self.store.write().expect("api tokens lock");
        store.by_token.insert(
            token.clone(),
            TokenEntry {
                org: org.clone(),
                source: TokenSource::Runtime {
                    id: id.clone(),
                    created_at: created_at.clone(),
                },
            },
        );
        store.id_to_token.insert(id.clone(), token.clone());
        IssuedToken {
            token,
            id,
            org,
            created_at,
        }
    }

    /// Revoke a runtime-issued token by id. Returns `true` iff the id
    /// belonged to `org` AND the token was runtime-issued (env tokens
    /// can't be revoked at runtime). Idempotent: revoking an unknown id
    /// returns `false` without side effects.
    pub fn revoke(&self, id: &TokenId, org: &OrgId) -> bool {
        let mut store = self.store.write().expect("api tokens lock");
        let Some(token) = store.id_to_token.get(id).cloned() else {
            return false;
        };
        let Some(entry) = store.by_token.get(&token) else {
            // Inconsistent state — id_to_token outlived by_token. Clean up.
            store.id_to_token.remove(id);
            return false;
        };
        if &entry.org != org {
            return false;
        }
        if !matches!(entry.source, TokenSource::Runtime { .. }) {
            return false;
        }
        store.by_token.remove(&token);
        store.id_to_token.remove(id);
        true
    }

    /// List runtime-issued tokens belonging to `org`. Env tokens are
    /// omitted (the operator manages them out of band). Order is
    /// implementation-defined — callers that need a deterministic shape
    /// must sort.
    pub fn list_runtime(&self, org: &OrgId) -> Vec<RuntimeTokenInfo> {
        let store = self.store.read().expect("api tokens lock");
        store
            .by_token
            .values()
            .filter_map(|entry| {
                if &entry.org != org {
                    return None;
                }
                let TokenSource::Runtime { id, created_at } = &entry.source else {
                    return None;
                };
                Some(RuntimeTokenInfo {
                    id: id.clone(),
                    org: entry.org.clone(),
                    created_at: created_at.clone(),
                })
            })
            .collect()
    }
}

/// Read 32 bytes from `/dev/urandom` and base64url-encode (no padding).
/// Yields a 43-char string prefixed with `nv_` so operators can grep for
/// nanovm bearer tokens at a glance. Total length: 46 chars.
fn generate_token() -> String {
    let bytes = read_urandom::<32>();
    let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    format!("nv_{enc}")
}

/// Read 16 bytes from `/dev/urandom` and base64url-encode (no padding).
/// Yields a 22-char string prefixed with `nvk_` — used as the public
/// id of a runtime-issued token in API responses.
fn generate_token_id() -> String {
    let bytes = read_urandom::<16>();
    let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    format!("nvk_{enc}")
}

/// Pull `N` random bytes from `/dev/urandom`. Panics on I/O failure —
/// the same posture every other Linux server takes, since a broken
/// `/dev/urandom` means the kernel is in a state nothing recovers from.
fn read_urandom<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    let mut f = File::open("/dev/urandom").expect("open /dev/urandom");
    f.read_exact(&mut buf).expect("read /dev/urandom");
    buf
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
    fn from_csv_empty_org_id_falls_back_to_default() {
        // `:tok` used to land an OrgId("") in extensions, which would
        // then appear as an empty-string label value in /metrics
        // (`nanovm_forks_total_by_org{org=""}`). Treat it as the
        // default-org form instead.
        let t = ApiTokens::from_csv(":tok1, :tok2");
        assert_eq!(t.org_for("tok1"), Some(OrgId::default_org()));
        assert_eq!(t.org_for("tok2"), Some(OrgId::default_org()));
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

    #[test]
    fn issue_returns_unique_tokens_and_authenticates() {
        let t = ApiTokens::from_csv("");
        let acme = OrgId::new("acme");
        let a = t.issue(acme.clone());
        let b = t.issue(acme.clone());
        assert_ne!(a.token, b.token, "tokens must be unique");
        assert_ne!(a.id, b.id, "ids must be unique");
        assert!(a.token.starts_with("nv_"));
        assert!(a.id.as_str().starts_with("nvk_"));
        assert_eq!(t.org_for(&a.token), Some(acme.clone()));
        assert_eq!(t.org_for(&b.token), Some(acme));
    }

    #[test]
    fn list_runtime_returns_only_callers_tokens() {
        let t = ApiTokens::from_csv("");
        let acme = OrgId::new("acme");
        let globex = OrgId::new("globex");
        let a1 = t.issue(acme.clone());
        let _a2 = t.issue(acme.clone());
        let _g1 = t.issue(globex.clone());
        let acme_list = t.list_runtime(&acme);
        assert_eq!(acme_list.len(), 2);
        assert!(acme_list.iter().all(|i| i.org == acme));
        let globex_list = t.list_runtime(&globex);
        assert_eq!(globex_list.len(), 1);
        assert_eq!(globex_list[0].org, globex);
        assert!(t.list_runtime(&OrgId::new("noone")).is_empty());
        // The issued ids still authenticate after listing.
        assert_eq!(t.org_for(&a1.token), Some(acme));
    }

    #[test]
    fn revoke_runtime_token_then_org_for_misses() {
        let t = ApiTokens::from_csv("");
        let acme = OrgId::new("acme");
        let a = t.issue(acme.clone());
        assert!(t.org_for(&a.token).is_some());
        assert!(t.revoke(&a.id, &acme));
        assert!(t.org_for(&a.token).is_none());
        // Idempotent: revoking again is a no-op false.
        assert!(!t.revoke(&a.id, &acme));
    }

    #[test]
    fn revoke_cross_org_is_rejected() {
        let t = ApiTokens::from_csv("");
        let acme = OrgId::new("acme");
        let globex = OrgId::new("globex");
        let a = t.issue(acme.clone());
        // Globex can't revoke acme's token.
        assert!(!t.revoke(&a.id, &globex));
        // Token still works.
        assert_eq!(t.org_for(&a.token), Some(acme));
    }

    #[test]
    fn revoke_env_token_is_rejected() {
        // Env tokens are operator-managed; runtime revoke must refuse.
        let t = ApiTokens::from_csv("acme:env-tok");
        // We don't expose a TokenId for env tokens, so revoke can't
        // even be addressed at one — but defensively try with a fake.
        let fake = TokenId("nvk_imaginary".into());
        assert!(!t.revoke(&fake, &OrgId::new("acme")));
        assert!(t.accepts("env-tok"));
    }
}
