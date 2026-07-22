//! Bearer-token authentication and per-org scoping for the control plane.
//!
//! Every accepted token belongs to exactly one org. On a successful
//! `require_token` pass, the middleware injects the calling `OrgId` into the
//! request's extensions; handlers read it to scope reads (list filters) and
//! writes (ownership inserts + cross-org 403s on subsequent ops).
//!
//! ## Env format
//!
//! `NANOVM_API_TOKENS` is a comma-separated list. Each entry is one of:
//!
//! - `token`                  → default org, `admin` role (legacy shape).
//! - `org_id:token`           → given org, `admin` role.
//! - `org_id:token@role`      → given org + explicit role. Roles are
//!   `admin` / `developer` / `viewer` (case-insensitive).
//!
//! **Why `@role` and not `:role`?** A `:` inside the token secret
//! (like `sk-live:abc123`) used to collide with a `:role` suffix if
//! the tail happened to match a role name — silently truncating the
//! token and breaking auth. `@` is not a valid role-name character,
//! so `sk-live:abc123` unambiguously parses as the full token while
//! `sk-live:abc123@admin` unambiguously means "with admin role".
//!
//! The legacy comma-only shape (`tok1,tok2`) keeps working: every legacy
//! token lands in the `default` org as `admin`. That preserves
//! single-tenant deployments byte-for-byte.
//!
//! The role is a **shovel-ready stub** — see [`Role`] and
//! [`require_role`]. No handler enforces it today; the plumbing lands
//! now so SSO integration doesn't need a schema migration.
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
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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

/// Coarse authorization role attached to every token. Not enforced by
/// any handler yet — this is a **shovel-ready stub** that lets the
/// schema, wire format, and extension-injection surface land now, so
/// that when SSO / group-attribute mapping arrives the plumbing is
/// already in place.
///
/// Ordering matters: `require_role(min)` uses `>=` on the ordinal so
/// a stricter role always satisfies a looser requirement. The order is
/// `Viewer < Developer < Admin`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, Ord, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Read-only. Can list VMs / snapshots / usage but not mutate.
    Viewer,
    /// Everyday developer scope: create/destroy VMs, fork snapshots,
    /// run exec. Cannot mint or revoke API keys, cannot touch billing.
    Developer,
    /// Everything: manage keys, billing portal, org-level settings.
    /// Every legacy env-loaded token defaults to `Admin` — the
    /// backward-compat posture.
    Admin,
}

impl Role {
    /// Default role for tokens whose source doesn't specify one
    /// (legacy env format, runtime-issued keys, mock/dev deploys).
    /// Chosen as `Admin` so existing single-tenant deployments keep
    /// working without config changes. A follow-up PR flips the
    /// default once SSO ships and roles start coming from group
    /// attributes.
    pub fn default_for_legacy() -> Self {
        Self::Admin
    }

    /// Parse from the wire / env format. Case-insensitive. Returns
    /// `None` on unknown strings — the env parser treats "unknown
    /// role suffix" as "no role suffix present" so a typo can't
    /// silently privilege-escalate.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "admin" => Some(Self::Admin),
            "developer" | "dev" => Some(Self::Developer),
            "viewer" | "readonly" | "read-only" => Some(Self::Viewer),
            _ => None,
        }
    }

    /// Stable machine-readable name — matches the `#[serde(rename_all)]`
    /// output so JSON, env, and audit-log formats round-trip.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Developer => "developer",
            Self::Viewer => "viewer",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Metadata for one accepted token. We never store the plaintext on this
/// struct (it lives only as the HashMap key for the auth lookup path).
#[derive(Clone, Debug)]
struct TokenEntry {
    org: OrgId,
    /// Coarse authorization role. Injected into request extensions
    /// alongside [`OrgId`] on every authenticated request. Defaults
    /// to [`Role::default_for_legacy`] (`Admin`) for env-loaded and
    /// runtime-issued tokens; SSO-provisioned tokens will populate
    /// this from IdP group membership when that ships.
    role: Role,
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
///
/// Persistence: when [`with_persistence`](Self::with_persistence) is
/// called, runtime-issued tokens are snapshotted to that path after
/// every `issue` / `revoke`. On startup, [`load_persisted`](
/// Self::load_persisted) replays the file so single-replica deployments
/// keep working across restarts. Env-loaded tokens are NOT persisted —
/// they're operator-managed via `NANOVM_API_TOKENS` and re-loaded on
/// every startup anyway.
#[derive(Debug, Default)]
pub struct ApiTokens {
    store: RwLock<Store>,
    /// Path of the JSON file backing runtime token persistence. `None`
    /// → in-memory only (the default; runtime keys are lost on restart).
    persist_path: Option<PathBuf>,
}

/// Wire shape of a persisted runtime token. Stored as a JSON array
/// `[{token, id, org, created_at}]` at the path configured by
/// `NANOVM_TOKEN_STORE_PATH`. Plaintext on disk is acceptable because
/// the file lives under the same threat model as `NANOVM_API_TOKENS`
/// (Helm Secret / Fly secret / mounted Kubernetes Secret) — anyone who
/// can read the file can already read the env that owns it.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedToken {
    token: String,
    id: String,
    org: String,
    created_at: String,
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
                    role: Role::default_for_legacy(),
                    source: TokenSource::Env,
                },
            );
        }
        Self {
            store: RwLock::new(store),
            persist_path: None,
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
                    role: Role::default_for_legacy(),
                    source: TokenSource::Env,
                },
            );
        }
        Self {
            store: RwLock::new(store),
            persist_path: None,
        }
    }

    /// Build from `NANOVM_API_TOKENS`. Empty / unset → "auth disabled".
    /// Also reads `NANOVM_TOKEN_STORE_PATH` and, when set, wires the
    /// persistence shim + replays any previously-issued runtime tokens
    /// so they survive process restarts.
    pub fn from_env() -> Self {
        let raw = std::env::var("NANOVM_API_TOKENS").unwrap_or_default();
        let mut tokens = Self::from_csv(&raw);
        if let Ok(path) = std::env::var("NANOVM_TOKEN_STORE_PATH") {
            if !path.is_empty() {
                tokens = tokens.with_persistence(PathBuf::from(path));
            }
        }
        tokens
    }

    /// Enable disk persistence of runtime-issued tokens at `path`.
    /// Replays the existing file (if any) into the accept set before
    /// returning. A missing file is fine (first-run); a malformed file
    /// is logged at `WARN` and skipped — the deployment stays usable.
    pub fn with_persistence(mut self, path: PathBuf) -> Self {
        self.persist_path = Some(path.clone());
        if let Err(err) = self.load_persisted(&path) {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "failed to load persisted runtime tokens; continuing with env tokens only"
            );
        }
        self
    }

    /// Replay the persistence file into the accept set. Treats a
    /// missing file as "no tokens yet" — only IO / JSON errors bubble
    /// up. Idempotent: re-loading the same file is a no-op.
    fn load_persisted(&self, path: &Path) -> std::io::Result<()> {
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut buf = String::new();
        file.read_to_string(&mut buf)?;
        if buf.trim().is_empty() {
            return Ok(());
        }
        let entries: Vec<PersistedToken> = serde_json::from_str(&buf).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("token store JSON: {e}"),
            )
        })?;
        let mut store = self.store.write().expect("api tokens lock");
        let mut loaded = 0usize;
        for p in entries {
            if p.token.is_empty() || p.id.is_empty() {
                continue;
            }
            let id = TokenId(p.id);
            store.by_token.insert(
                p.token.clone(),
                TokenEntry {
                    org: OrgId(p.org),
                    // Persisted-runtime tokens predate the role field;
                    // treat them as Admin (matches env-loaded default)
                    // until the persistence format gains a role column.
                    role: Role::default_for_legacy(),
                    source: TokenSource::Runtime {
                        id: id.clone(),
                        created_at: p.created_at,
                    },
                },
            );
            store.id_to_token.insert(id, p.token);
            loaded += 1;
        }
        if loaded > 0 {
            tracing::info!(
                count = loaded,
                path = %path.display(),
                "replayed runtime tokens from persistence file"
            );
        }
        Ok(())
    }

    /// Snapshot the runtime token set to the persistence file. Writes
    /// to a sibling `*.tmp` first and `rename`s into place so a crash
    /// mid-write doesn't corrupt the file an operator can read.
    /// Best-effort: errors are logged at `WARN` (the in-memory state
    /// is still authoritative for the running process).
    fn persist(&self, store: &Store) {
        let Some(path) = self.persist_path.as_ref() else {
            return;
        };
        let entries: Vec<PersistedToken> = store
            .by_token
            .iter()
            .filter_map(|(token, entry)| match &entry.source {
                TokenSource::Runtime { id, created_at } => Some(PersistedToken {
                    token: token.clone(),
                    id: id.0.clone(),
                    org: entry.org.0.clone(),
                    created_at: created_at.clone(),
                }),
                TokenSource::Env => None,
            })
            .collect();
        let payload = match serde_json::to_vec_pretty(&entries) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "serialize runtime tokens");
                return;
            }
        };
        let tmp = path.with_extension("tmp");
        let write = (|| -> std::io::Result<()> {
            let mut f = File::create(&tmp)?;
            f.write_all(&payload)?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)
        })();
        if let Err(e) = write {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "persist runtime tokens to disk"
            );
        }
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
            let (org, token, role) = parse_env_entry(trimmed);
            if token.is_empty() {
                continue;
            }
            // Later duplicates overwrite earlier ones; the env shape is
            // operator-controlled, so we keep the last wins rule explicit.
            store.by_token.insert(
                token,
                TokenEntry {
                    org,
                    role,
                    source: TokenSource::Env,
                },
            );
        }
        Self {
            store: RwLock::new(store),
            persist_path: None,
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

    /// Returns the `(org, role)` pair the presented token maps to, or
    /// `None` if the token isn't in the set. Used by [`require_token`]
    /// to inject both extensions in one lookup. Handlers that only
    /// need the org can keep using [`org_for`](Self::org_for); anything
    /// that will grow role checks eventually should switch to this.
    pub fn resolve(&self, presented: &str) -> Option<(OrgId, Role)> {
        self.store
            .read()
            .expect("api tokens lock")
            .by_token
            .get(presented)
            .map(|e| (e.org.clone(), e.role))
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
                // Runtime-issued keys default to Admin. `POST /v1/keys`
                // gains an optional `role` body field in the follow-up
                // that ships SSO — until then, self-serve dashboards
                // that mint keys assume full org privilege.
                role: Role::default_for_legacy(),
                source: TokenSource::Runtime {
                    id: id.clone(),
                    created_at: created_at.clone(),
                },
            },
        );
        store.id_to_token.insert(id.clone(), token.clone());
        self.persist(&store);
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
        self.persist(&store);
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

/// Parse one comma-separated entry from `NANOVM_API_TOKENS` into
/// `(org, token, role)`. Accepted shapes:
///
/// - `tok`                    → `(default, tok, admin)` — legacy.
/// - `org:tok`                → `(org,     tok, admin)` — multi-tenant.
/// - `org:tok@role`           → `(org,     tok, role)`  — RBAC stub.
///
/// The `@role` suffix is **only recognised on the LAST field of the
/// entry** (i.e. on the token). We use `@` rather than a third `:`
/// specifically to prevent the ambiguity Copilot flagged on #180 —
/// a token secret that legitimately contains `:` (e.g. `sk-live:abc`)
/// used to be silently truncated when the tail happened to match a
/// role name. `@` is not a legal role-name character AND is highly
/// unusual inside opaque bearer tokens, so `tok@admin` is
/// unambiguously "token `tok` with role `admin`" while `sk-live:abc`
/// keeps parsing as the full token.
///
/// If a token legitimately contains `@` followed by a known role
/// name (e.g. `hunter2@admin`), the split still fires — same trade
/// as any prefix-scheme parser. Operators that hit that edge case
/// should base64url-encode the secret or rotate the token; the
/// alternative would be requiring `@role` at a specific position that
/// can never appear inside the token, which no token generator
/// guarantees.
///
/// Empty org (`:tok`) still folds to `default`.
///
/// Split out from [`ApiTokens::from_csv`] so unit tests can exercise
/// the parser without HashMap ceremony.
fn parse_env_entry(entry: &str) -> (OrgId, String, Role) {
    let entry = entry.trim();
    // First split off the optional role suffix from the RIGHT so the
    // token side keeps every `:` intact.
    let (body, role) = split_role_suffix(entry);
    // Then split org:token from the body.
    let (org, token) = match body.split_once(':') {
        Some((org, tok)) => (normalize_org(org), tok.trim().to_owned()),
        None => (OrgId::default_org(), body.trim().to_owned()),
    };
    (org, token, role)
}

/// Split an entry on a trailing `@<role>` marker. Returns
/// `(body_without_role, role)` — `body` == input if no suffix is
/// found or the tail isn't a known role name. Deliberately splits
/// from the RIGHT so an org id containing `@` (unusual but legal)
/// isn't affected.
fn split_role_suffix(entry: &str) -> (&str, Role) {
    match entry.rsplit_once('@') {
        Some((body, tail)) => match Role::parse(tail) {
            Some(role) => (body, role),
            None => (entry, Role::default_for_legacy()),
        },
        None => (entry, Role::default_for_legacy()),
    }
}

/// Parse an `Authorization` header value into the bearer token. RFC
/// 7235 § 5.1: auth-scheme names are **case-insensitive** and the
/// scheme may be separated from the credentials by 1+ SP or HTAB.
/// Previously we used `strip_prefix("Bearer ")`, which rejected
/// `bearer <tok>` / `BEARER  <tok>` / `Bearer\t<tok>` — all valid per
/// the spec. Now: split off the first whitespace-delimited word,
/// case-fold, verify `bearer`, then trim any additional leading
/// whitespace off the credential.
///
/// Returns `None` when the header isn't a bearer challenge — the
/// middleware turns that into a 401.
fn parse_bearer_scheme(header: &str) -> Option<&str> {
    let header = header.trim_start();
    let (scheme, rest) = header.split_once(|c: char| c.is_ascii_whitespace())?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim_start();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

fn normalize_org(org_str: &str) -> OrgId {
    let s = org_str.trim();
    if s.is_empty() {
        OrgId::default_org()
    } else {
        OrgId::new(s)
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
        // Role is Admin (least-restrictive) so auth-off mode keeps
        // every handler reachable.
        req.extensions_mut().insert(OrgId::default_org());
        req.extensions_mut().insert(Role::default_for_legacy());
        return Ok(next.run(req).await);
    }
    let bearer = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(parse_bearer_scheme)
        .ok_or_else(|| {
            ApiError::Unauthorized("missing or malformed authorization header".into())
        })?;
    let Some((org, role)) = tokens.resolve(bearer) else {
        return Err(ApiError::Unauthorized("invalid api token".into()));
    };
    req.extensions_mut().insert(org);
    req.extensions_mut().insert(role);
    Ok(next.run(req).await)
}

/// Handler-side helper: verify the caller's [`Role`] extension is at
/// least `min`. Returns `Err(ApiError::Forbidden)` with a
/// `role_required` code on insufficient scope, `Ok(())` otherwise.
///
/// **Not called by any handler today** — this is the shovel-ready
/// hook for when SSO ships and role assignments arrive from group
/// mapping. Kept next to the [`Role`] type so the enforcement API
/// lives with the schema.
///
/// Typical use once SSO lands:
///
/// ```ignore
/// async fn revoke_key(
///     Extension(role): Extension<Role>,
///     // ...
/// ) -> Result<(), ApiError> {
///     require_role(role, Role::Admin)?;
///     // ...
/// }
/// ```
// Intentionally unused today — see the doc comment above. The stub
// exists so handlers can start calling it the moment SSO ships without
// a follow-up refactor. Kept `pub(crate)` so it survives dead-code
// analysis in the whole crate; when the first handler calls it the
// allow can come off.
#[allow(dead_code)]
pub(crate) fn require_role(caller: Role, min: Role) -> Result<(), ApiError> {
    if caller >= min {
        Ok(())
    } else {
        Err(ApiError::Forbidden {
            code: "role_required",
            message: format!("this endpoint requires role >= {min}; caller has {caller}"),
        })
    }
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

    // ---- RBAC stub -----------------------------------------------------

    #[test]
    fn role_parse_case_insensitive() {
        assert_eq!(Role::parse("admin"), Some(Role::Admin));
        assert_eq!(Role::parse("ADMIN"), Some(Role::Admin));
        assert_eq!(Role::parse("Developer"), Some(Role::Developer));
        assert_eq!(Role::parse("dev"), Some(Role::Developer));
        assert_eq!(Role::parse("viewer"), Some(Role::Viewer));
        assert_eq!(Role::parse("readonly"), Some(Role::Viewer));
        assert_eq!(Role::parse("read-only"), Some(Role::Viewer));
        assert_eq!(Role::parse("  viewer  "), Some(Role::Viewer));
        assert_eq!(Role::parse("root"), None);
        assert_eq!(Role::parse(""), None);
    }

    #[test]
    fn role_ordering_admin_is_strictest() {
        // Ordering matters for require_role's `caller >= min` check.
        assert!(Role::Admin >= Role::Developer);
        assert!(Role::Admin >= Role::Viewer);
        assert!(Role::Developer >= Role::Viewer);
        assert!(Role::Developer < Role::Admin);
        assert!(Role::Viewer < Role::Developer);
    }

    #[test]
    fn require_role_allows_stricter_and_rejects_looser() {
        assert!(require_role(Role::Admin, Role::Developer).is_ok());
        assert!(require_role(Role::Developer, Role::Developer).is_ok());
        assert!(require_role(Role::Admin, Role::Admin).is_ok());
        let err = require_role(Role::Viewer, Role::Admin).unwrap_err();
        assert!(matches!(
            err,
            ApiError::Forbidden {
                code: "role_required",
                ..
            }
        ));
    }

    #[test]
    fn parse_env_entry_shapes() {
        // Legacy: no colons at all.
        assert_eq!(
            parse_env_entry("just-a-token"),
            (
                OrgId::default_org(),
                "just-a-token".to_string(),
                Role::Admin
            )
        );
        // Two-part: org:token, default role.
        assert_eq!(
            parse_env_entry("acme:tok"),
            (OrgId::new("acme"), "tok".to_string(), Role::Admin)
        );
        // Three-part with @role suffix.
        assert_eq!(
            parse_env_entry("acme:tok@developer"),
            (OrgId::new("acme"), "tok".to_string(), Role::Developer)
        );
        assert_eq!(
            parse_env_entry("acme:tok@viewer"),
            (OrgId::new("acme"), "tok".to_string(), Role::Viewer)
        );
        // Empty org folds to default.
        assert_eq!(
            parse_env_entry(":tok@admin"),
            (OrgId::default_org(), "tok".to_string(), Role::Admin)
        );
        // Legacy shape with @role (no org prefix).
        assert_eq!(
            parse_env_entry("legacy-tok@viewer"),
            (OrgId::default_org(), "legacy-tok".to_string(), Role::Viewer)
        );
    }

    #[test]
    fn parse_env_entry_preserves_colons_inside_token_secret() {
        // A token secret with `:` inside it (e.g. `sk-live:abc`) MUST
        // parse as-is. The old `:role` split silently mangled this.
        let (org, tok, role) = parse_env_entry("acme:sk-live:abc");
        assert_eq!(org, OrgId::new("acme"));
        assert_eq!(tok, "sk-live:abc");
        assert_eq!(role, Role::Admin);

        // Even the pathological case from the review comment now works:
        // a token whose plaintext ends with `:admin`.
        let (org, tok, role) = parse_env_entry("acme:sk-live:admin");
        assert_eq!(org, OrgId::new("acme"));
        assert_eq!(tok, "sk-live:admin");
        assert_eq!(role, Role::Admin);

        // Same token WITH explicit @role suffix — unambiguous.
        let (org, tok, role) = parse_env_entry("acme:sk-live:admin@developer");
        assert_eq!(org, OrgId::new("acme"));
        assert_eq!(tok, "sk-live:admin");
        assert_eq!(role, Role::Developer);
    }

    #[test]
    fn parse_env_entry_unknown_role_suffix_falls_back() {
        // `@` present but tail isn't a known role → treat as part of
        // the token, not a role marker.
        let (org, tok, role) = parse_env_entry("acme:tok@notarole");
        assert_eq!(org, OrgId::new("acme"));
        assert_eq!(tok, "tok@notarole");
        assert_eq!(role, Role::Admin);
    }

    #[test]
    fn from_csv_admits_role_suffix() {
        let t = ApiTokens::from_csv("acme:tok-a@admin,acme:tok-b@developer,acme:tok-c@viewer");
        assert_eq!(t.len(), 3);
        assert_eq!(t.resolve("tok-a").map(|(_, r)| r), Some(Role::Admin));
        assert_eq!(t.resolve("tok-b").map(|(_, r)| r), Some(Role::Developer));
        assert_eq!(t.resolve("tok-c").map(|(_, r)| r), Some(Role::Viewer));
    }

    #[test]
    fn parse_bearer_scheme_is_case_insensitive_and_tolerates_whitespace() {
        // Canonical form.
        assert_eq!(parse_bearer_scheme("Bearer tok"), Some("tok"));
        // Lowercase scheme (RFC 7235 § 5.1 allows).
        assert_eq!(parse_bearer_scheme("bearer tok"), Some("tok"));
        // All-caps scheme.
        assert_eq!(parse_bearer_scheme("BEARER tok"), Some("tok"));
        // MixedCase.
        assert_eq!(parse_bearer_scheme("BeArEr tok"), Some("tok"));
        // Multiple spaces + tab between scheme and credential.
        assert_eq!(parse_bearer_scheme("Bearer   tok"), Some("tok"));
        assert_eq!(parse_bearer_scheme("Bearer\ttok"), Some("tok"));
        // Leading whitespace on the header value.
        assert_eq!(parse_bearer_scheme("  Bearer tok"), Some("tok"));
        // Wrong scheme.
        assert_eq!(parse_bearer_scheme("Basic dXNlcjpwYXNz"), None);
        // Missing credential.
        assert_eq!(parse_bearer_scheme("Bearer "), None);
        assert_eq!(parse_bearer_scheme("Bearer"), None);
    }

    #[test]
    fn resolve_returns_admin_for_legacy_token_shape() {
        let t = ApiTokens::from_csv("legacy-token,acme:tok");
        assert_eq!(
            t.resolve("legacy-token"),
            Some((OrgId::default_org(), Role::Admin))
        );
        assert_eq!(t.resolve("tok"), Some((OrgId::new("acme"), Role::Admin)));
    }

    #[test]
    fn role_round_trips_through_serde() {
        let json = serde_json::to_string(&Role::Developer).unwrap();
        assert_eq!(json, r#""developer""#);
        let back: Role = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Role::Developer);
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

    #[test]
    fn persistence_roundtrip_replays_runtime_tokens() {
        // First ApiTokens instance issues; second loads from disk and
        // sees the same tokens. Models a process restart with
        // NANOVM_TOKEN_STORE_PATH set.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");

        let acme = OrgId::new("acme");
        let issued = {
            let t = ApiTokens::from_csv("").with_persistence(path.clone());
            let a = t.issue(acme.clone());
            let _ = t.issue(OrgId::new("globex"));
            a
        };
        // File exists and is non-empty.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(&issued.token));
        assert!(raw.contains(issued.id.as_str()));

        // Fresh instance loads the file.
        let restored = ApiTokens::from_csv("").with_persistence(path.clone());
        assert_eq!(restored.org_for(&issued.token), Some(acme.clone()));
        assert_eq!(restored.list_runtime(&acme).len(), 1);
        assert_eq!(restored.list_runtime(&OrgId::new("globex")).len(), 1);
    }

    #[test]
    fn persistence_revoke_persists_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        let acme = OrgId::new("acme");

        let id = {
            let t = ApiTokens::from_csv("").with_persistence(path.clone());
            let a = t.issue(acme.clone());
            assert!(t.revoke(&a.id, &acme));
            a.id
        };

        // After revoke the token no longer exists on disk.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(id.as_str()));

        // A fresh instance loading the file sees no runtime tokens.
        let restored = ApiTokens::from_csv("").with_persistence(path);
        assert!(restored.list_runtime(&acme).is_empty());
    }

    #[test]
    fn persistence_missing_file_is_not_an_error() {
        // First-run case: path doesn't exist yet. with_persistence
        // should not panic; the accept set stays as whatever was
        // already there from env.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let t = ApiTokens::from_csv("acme:env-tok").with_persistence(path);
        assert!(t.accepts("env-tok"));
    }

    #[test]
    fn persistence_malformed_file_logs_warn_and_continues() {
        // A corrupted JSON file shouldn't take down startup. The env
        // tokens stay usable; the bad runtime file is simply ignored.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{ not valid json").unwrap();
        let t = ApiTokens::from_csv("acme:env-tok").with_persistence(path);
        assert!(t.accepts("env-tok"));
    }

    #[test]
    fn persistence_does_not_write_env_tokens() {
        // Env-loaded tokens must NOT end up in the persistence file —
        // operator-managed lifecycle.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        let t = ApiTokens::from_csv("acme:env-tok").with_persistence(path.clone());
        // No runtime tokens yet → no file written either (issue/revoke
        // are the only writers; env tokens skip persist).
        // After a runtime issue, the file should contain ONLY the
        // runtime entry, not env-tok.
        let _ = t.issue(OrgId::new("acme"));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("env-tok"));
    }
}
