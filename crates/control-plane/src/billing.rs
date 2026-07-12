//! Stripe-backed signup + billing portal foundations.
//!
//! Only compiled with `--features billing` (implies `sqlite`).
//!
//! ## What ships in this PR
//!
//! - [`StripeClient`] — thin `reqwest` wrapper around the two Stripe
//!   REST endpoints we need (`POST /v1/customers`, `POST /v1/billing_portal/sessions`).
//! - [`BillingStore`] trait + [`InMemoryBillingStore`] +
//!   [`SqliteBillingStore`] — persistence for the org → Stripe
//!   customer id mapping. Same SQLite file the ownership store uses.
//! - [`BillingConfig::from_env`] — reads `STRIPE_SECRET_KEY`,
//!   `STRIPE_BILLING_PORTAL_RETURN_URL`, `NANOVM_SIGNUP_TOKEN`.
//!   Returns `None` when any is unset so the binary boots without
//!   billing rather than crashing.
//! - [`signup`] / [`billing_portal`] handler *functions* that carry
//!   the semantic contract for `POST /v1/signup` and `GET /v1/billing/portal`.
//!
//! ## Deliberate follow-up
//!
//! Wiring these handlers into `AppState` + [`crate::router`] needs a
//! small refactor to attach a `BillingCtx` sub-state; kept as a
//! separate PR so this one stays focused on the Stripe surface + persistence.
//!
//! ## Env vars
//!
//! - `STRIPE_SECRET_KEY` — Stripe API secret (`sk_test_…` / `sk_live_…`).
//!   **Never commit**; wire via `flyctl secrets set` / Helm values / K8s Secret.
//! - `STRIPE_BILLING_PORTAL_RETURN_URL` — return URL for the Stripe portal.
//! - `NANOVM_SIGNUP_TOKEN` — admin bearer gating `POST /v1/signup` for MVP.

#![cfg(feature = "billing")]
// Enum variants are self-documenting via their `thiserror` display
// strings; per-variant docstrings would be redundant. The module
// itself has a rich file-level doc block above.
#![allow(missing_docs)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Json,
    Extension,
};
use serde::{Deserialize, Serialize};

use crate::auth::{ApiTokens, IssuedToken, OrgId};
use crate::error::ApiError;

// -------- DTOs ----------------------------------------------------------

/// `POST /v1/signup` request body.
#[derive(Debug, Deserialize)]
pub struct SignupRequest {
    /// Contact email for the new tenant. Passed to Stripe as the
    /// customer's `email` so invoices go to the right inbox.
    pub email: String,
    /// Human-readable org name. Also stored as Stripe customer `name`.
    /// **Not** the internal `OrgId` — the server derives that by
    /// slugifying so it's stable across renames.
    pub org: String,
}

/// `POST /v1/signup` response — plaintext API key returned once
/// on issue and never again.
#[derive(Debug, Serialize)]
pub struct SignupResponse {
    /// Stable internal org id (slug).
    pub org: String,
    /// First API token for the new org. Format: `<org>:<secret>`.
    pub api_key: String,
    /// Stripe customer id (`cus_…`).
    pub stripe_customer_id: String,
}

/// `GET /v1/billing/portal` response.
#[derive(Debug, Serialize)]
pub struct PortalResponse {
    /// Short-lived Stripe portal URL.
    pub url: String,
}

/// `POST /v1/signup/request` request body — the self-serve entrypoint.
/// No admin bearer required; server-side rate-limited per IP.
#[derive(Debug, Deserialize)]
pub struct SignupRequestRequest {
    /// Email to send the magic link to. Also becomes the Stripe
    /// customer's `email`.
    pub email: String,
    /// Human-readable org name. Slugified server-side to the internal
    /// `OrgId` on activation.
    pub org: String,
}

/// `POST /v1/signup/request` response — deliberately opaque about
/// whether the email exists. The client is told "if that address is
/// eligible, check your inbox" regardless of validity so an attacker
/// can't enumerate registered addresses by watching the response.
#[derive(Debug, Serialize)]
pub struct SignupRequestResponse {
    /// Constant string. Always the same value regardless of outcome.
    pub message: String,
}

/// `POST /v1/signup/verify` request body — carries the magic-link token
/// the user pasted from their email.
#[derive(Debug, Deserialize)]
pub struct SignupVerifyRequest {
    /// The raw token from the magic-link URL. Server hashes with
    /// SHA-256 to look up the pending signup.
    pub token: String,
}

// -------- Plan tiers ---------------------------------------------------

/// A named plan tier — Stripe `price_id` → human name + effective
/// fork-quota rate (requests per second). The follow-up "wire into
/// ForkQuota" PR will consult `rps` when checking a caller's org.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanTier {
    /// Human name shown on the dashboard (e.g. `"pro"`, `"enterprise"`).
    pub name: String,
    /// Sustained rate limit in forks/second.
    pub rps: u32,
}

/// `NANOVM_PLAN_TIERS` config. Format:
/// `price_ABC=free:5,price_XYZ=pro:100,price_ENT=enterprise:1000`.
///
/// Each triple is `stripe_price_id=name:rps`; separators between
/// triples are commas; the parser is intentionally permissive on
/// whitespace + trailing commas. Callers with no mapped tier fall
/// through to the env default configured on
/// [`crate::ForkQuota`] (`NANOVM_FORK_RPS` / `NANOVM_FORK_BURST`) —
/// that fallback is applied in `try_acquire_org`, not here.
#[derive(Debug, Clone, Default)]
pub struct PlanTiers {
    /// `price_id → tier`
    by_price_id: HashMap<String, PlanTier>,
}

impl PlanTiers {
    /// Parse `NANOVM_PLAN_TIERS` if set. Unset → empty map (no tiers
    /// configured; the fork-quota fallback still applies). Malformed
    /// entries are logged and skipped so a typo doesn't crash boot.
    pub fn from_env() -> Self {
        let raw = std::env::var("NANOVM_PLAN_TIERS").unwrap_or_default();
        Self::parse(&raw)
    }

    /// Same as [`from_env`](Self::from_env) but takes the raw string
    /// directly — useful in tests.
    pub fn parse(raw: &str) -> Self {
        let mut by_price_id = HashMap::new();
        for entry in raw.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let Some((price_id, tail)) = entry.split_once('=') else {
                tracing::warn!(entry, "NANOVM_PLAN_TIERS: skipping entry without `=`");
                continue;
            };
            let Some((name, rps)) = tail.split_once(':') else {
                tracing::warn!(entry, "NANOVM_PLAN_TIERS: skipping entry without `:rps`");
                continue;
            };
            let Ok(rps) = rps.trim().parse::<u32>() else {
                tracing::warn!(entry, "NANOVM_PLAN_TIERS: rps not a u32; skipping");
                continue;
            };
            let price_id = price_id.trim();
            let name = name.trim();
            if price_id.is_empty() || name.is_empty() {
                tracing::warn!(
                    entry,
                    "NANOVM_PLAN_TIERS: empty price_id or tier name; skipping"
                );
                continue;
            }
            by_price_id.insert(
                price_id.to_string(),
                PlanTier {
                    name: name.to_string(),
                    rps,
                },
            );
        }
        Self { by_price_id }
    }

    /// Number of configured tiers.
    pub fn len(&self) -> usize {
        self.by_price_id.len()
    }

    /// True when no tiers are configured (unset env var).
    pub fn is_empty(&self) -> bool {
        self.by_price_id.is_empty()
    }

    /// Look up a tier by Stripe price id.
    pub fn get(&self, price_id: &str) -> Option<&PlanTier> {
        self.by_price_id.get(price_id)
    }
}

/// The resolved billing plan for a caller, returned by
/// `GET /v1/billing/plan`. `plan` is `None` when the caller has no
/// subscription (never signed up, or their sub was deleted) — the
/// dashboard renders this as "Free".
#[derive(Debug, Serialize)]
pub struct PlanResponse {
    /// Named tier (e.g. `"pro"`), or `None` when the caller has no
    /// active subscription mapped to a configured tier.
    pub plan: Option<PlanTier>,
    /// Raw Stripe subscription status (`active`, `trialing`,
    /// `past_due`, `canceled`, …). `None` when no subscription event
    /// has been seen for this org.
    pub subscription_status: Option<String>,
    /// Stripe price id currently on file — useful for operators
    /// debugging why a subscription didn't map to a named tier
    /// (typo in `NANOVM_PLAN_TIERS`? new price id?).
    pub price_id: Option<String>,
}

/// Resolve the current plan for an org: look up its Stripe customer
/// id, fetch its subscription state, map the price_id via
/// [`PlanTiers`]. All three lookups are cheap and stateless.
pub fn resolve_plan(tiers: &PlanTiers, store: &dyn BillingStore, org: &OrgId) -> PlanResponse {
    let customer_id = match store.get_customer(org) {
        Some(cid) => cid,
        None => {
            return PlanResponse {
                plan: None,
                subscription_status: None,
                price_id: None,
            };
        }
    };
    let sub = match store.get_subscription(&customer_id) {
        Some(s) => s,
        None => {
            return PlanResponse {
                plan: None,
                subscription_status: None,
                price_id: None,
            };
        }
    };
    let plan = sub
        .price_id
        .as_deref()
        .and_then(|pid| tiers.get(pid))
        .cloned();
    PlanResponse {
        plan,
        subscription_status: Some(sub.status),
        price_id: sub.price_id,
    }
}

// -------- BillingConfig ------------------------------------------------

/// Runtime billing configuration. All three fields are required — a
/// half-configured deploy would surface as opaque 500s on the first
/// signup, so we prefer to boot without the feature entirely and
/// return 503 `billing_disabled` to would-be signups.
#[derive(Clone)]
pub struct BillingConfig {
    /// Stripe API secret key — `sk_test_…` in dev, `sk_live_…` in
    /// prod. Read from `STRIPE_SECRET_KEY`. Never appears in Debug
    /// output — see the manual `Debug` impl.
    pub stripe_secret_key: String,
    /// Return URL Stripe's hosted billing portal sends the customer
    /// back to after they finish managing their subscription /
    /// card / invoices. Typically the SaaS dashboard URL.
    /// Read from `STRIPE_BILLING_PORTAL_RETURN_URL`.
    pub portal_return_url: String,
    /// Admin bearer token that gates `POST /v1/signup`. Rotate on
    /// demand. Read from `NANOVM_SIGNUP_TOKEN`. Never appears in
    /// Debug output.
    pub signup_token: String,
    /// Stripe webhook signing secret (`whsec_…`) — read from
    /// `STRIPE_WEBHOOK_SIGNING_SECRET`. Used to verify the
    /// `Stripe-Signature` header on `POST /v1/stripe/webhook`.
    /// `None` when unset, in which case the webhook endpoint returns
    /// 501 `webhook_disabled` (via [`crate::error::ApiError::Unsupported`]).
    /// Never appears in Debug output.
    pub webhook_signing_secret: Option<String>,
    /// Base URL the magic-link email points at. The signup handler
    /// appends `?token=<raw>` and sends the result. Read from
    /// `NANOVM_SIGNUP_VERIFY_URL`; typically
    /// `https://app.your-saas.com/signup/verify`. Defaults to
    /// `http://localhost:8080/v1/signup/verify` for dev.
    pub signup_verify_url: String,
    /// Magic-link token lifetime in seconds. Read from
    /// `NANOVM_SIGNUP_TOKEN_TTL_SECS`. Defaults to 900 (15 min) —
    /// short enough to limit exposure of a leaked email, long enough
    /// to survive a lazy inbox client.
    pub signup_token_ttl_secs: i64,
}

impl std::fmt::Debug for BillingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BillingConfig")
            .field("stripe_secret_key", &"<redacted>")
            .field("portal_return_url", &self.portal_return_url)
            .field("signup_token", &"<redacted>")
            .field(
                "webhook_signing_secret",
                &self.webhook_signing_secret.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl BillingConfig {
    /// `None` when any of the three core env vars is unset. The
    /// webhook signing secret is optional — it's read separately
    /// because a deploy can want `/v1/signup` + `/v1/billing/portal`
    /// live without accepting webhooks yet.
    pub fn from_env() -> Option<Self> {
        Some(Self {
            stripe_secret_key: std::env::var("STRIPE_SECRET_KEY").ok()?,
            portal_return_url: std::env::var("STRIPE_BILLING_PORTAL_RETURN_URL").ok()?,
            signup_token: std::env::var("NANOVM_SIGNUP_TOKEN").ok()?,
            webhook_signing_secret: std::env::var("STRIPE_WEBHOOK_SIGNING_SECRET")
                .ok()
                .filter(|s| !s.is_empty()),
            // Default points at the DASHBOARD's client-side verify
            // page, NOT the server's POST endpoint. A magic-link click
            // is a `GET` from the user's browser; landing on
            // `POST /v1/signup/verify` would 405. The dashboard page
            // handles the click, reads the `?token=` query, and POSTs
            // to the server endpoint on the user's behalf.
            signup_verify_url: std::env::var("NANOVM_SIGNUP_VERIFY_URL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "http://localhost:3000/signup/verify".into()),
            signup_token_ttl_secs: std::env::var("NANOVM_SIGNUP_TOKEN_TTL_SECS")
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(900),
        })
    }
}

// -------- Errors -------------------------------------------------------

/// Errors from the billing subsystem. Handler wiring in a follow-up
/// PR maps these to the appropriate [`crate::error::ApiError`] variant.
#[derive(Debug, thiserror::Error)]
pub enum BillingError {
    #[error("billing endpoints are not configured on this deployment")]
    Disabled,
    #[error("invalid signup token")]
    BadSignupToken,
    #[error("org name must contain at least one alphanumeric character")]
    InvalidOrg,
    #[error("magic-link token is unknown or expired")]
    InvalidSignupToken,
    #[error("no stripe customer recorded for org {0:?}")]
    NoCustomerForOrg(String),
    #[error("stripe api error ({status}): {message}")]
    StripeApi { status: u16, message: String },
    #[error("stripe transport error: {0}")]
    StripeTransport(String),
    #[error("billing store: {0}")]
    Store(#[from] BillingStoreError),
}

// -------- BillingStore -------------------------------------------------

/// Persistence for the org → Stripe customer + subscription state.
pub trait BillingStore: Send + Sync + std::fmt::Debug {
    fn record_customer(&self, org: &OrgId, customer_id: &str) -> Result<(), BillingStoreError>;
    fn get_customer(&self, org: &OrgId) -> Option<String>;

    /// Record (or overwrite) subscription state for a given Stripe
    /// `customer_id`. Called from the webhook handler on any
    /// `customer.subscription.*` event.
    ///
    /// The lookup key is the Stripe customer id, not `OrgId`, because
    /// the webhook payload carries the customer id; the org lookup
    /// happens via `org_by_customer` when a caller wants the
    /// tenant-facing view.
    fn record_subscription(
        &self,
        customer_id: &str,
        state: &SubscriptionState,
    ) -> Result<(), BillingStoreError>;

    /// Look up the current subscription state for `customer_id`, if
    /// any. `None` when the customer was created but never had a
    /// subscription event.
    fn get_subscription(&self, customer_id: &str) -> Option<SubscriptionState>;

    /// Reverse lookup — find the org that owns a given Stripe
    /// `customer_id`. Used by the webhook handler to route
    /// `customer.subscription.*` back to the org whose fork quota
    /// (or feature toggles) should change. `None` when the customer
    /// isn't associated with any org (typically a data issue —
    /// signup succeeded on Stripe but crashed before persisting).
    fn org_by_customer(&self, customer_id: &str) -> Option<OrgId>;

    /// Record a pending self-serve signup. The token itself is never
    /// stored — only its SHA-256 hash — so a compromised backup can't
    /// be used to activate outstanding invitations. Called from
    /// `POST /v1/signup/request` after generating a fresh magic-link
    /// token.
    ///
    /// If a row with the same `token_hash` already exists (extremely
    /// unlikely for a 24-byte random token, but theoretically possible),
    /// the store must overwrite it so the fresh token wins. If a row
    /// with the same email exists but a different hash, it must be
    /// replaced so re-requesting a signup invalidates the prior token.
    fn record_pending_signup(&self, signup: &PendingSignup) -> Result<(), BillingStoreError>;

    /// Consume a pending signup by its `token_hash`. Returns `Some` and
    /// deletes the row iff it exists AND `expires_at` is still in the
    /// future relative to `now` (RFC 3339). Expired rows are treated as
    /// absent (and should be swept by `gc_expired_signups`). This is
    /// the atomic point that prevents a token being redeemed twice.
    ///
    /// **Prefer [`peek_pending_signup`](Self::peek_pending_signup) +
    /// [`delete_pending_signup`](Self::delete_pending_signup) for
    /// verify flows** so a Stripe failure mid-verify doesn't leave the
    /// user with a consumed token AND no customer.
    fn take_pending_signup(&self, token_hash: &str, now: &str) -> Option<PendingSignup>;

    /// Non-destructive lookup — same expiry semantics as
    /// [`take_pending_signup`](Self::take_pending_signup) but leaves
    /// the row in place. The caller is expected to follow up with
    /// [`delete_pending_signup`](Self::delete_pending_signup) once the
    /// downstream side effects (Stripe customer create, etc.) have
    /// succeeded.
    fn peek_pending_signup(&self, token_hash: &str, now: &str) -> Option<PendingSignup>;

    /// Finalize a signup by removing the pending row. Returns Ok even
    /// if the row was already gone (concurrent verify / GC).
    fn delete_pending_signup(&self, token_hash: &str) -> Result<(), BillingStoreError>;

    /// Delete pending-signup rows whose `expires_at` is < `now`. Called
    /// periodically by the control-plane. Returns the count removed.
    fn gc_expired_signups(&self, now: &str) -> Result<u64, BillingStoreError>;
}

/// Subscription state persisted per Stripe customer. Kept minimal —
/// the follow-up "tier-based fork quota" PR reads `status` + `price_id`
/// to decide the caller's per-org rate limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionState {
    /// Stripe subscription id (`sub_…`).
    pub subscription_id: String,
    /// Stripe's raw status string — one of `active`, `trialing`,
    /// `past_due`, `canceled`, `incomplete`, `incomplete_expired`,
    /// `paused`, `unpaid`. Stored verbatim so future Stripe
    /// additions round-trip without a code change.
    pub status: String,
    /// Stripe price id (`price_…`) of the primary subscription item.
    /// Maps to your product catalogue (free / pro / enterprise) by
    /// application config (`NANOVM_PLAN_TIERS`).
    pub price_id: Option<String>,
    /// Stripe **subscription item** id (`si_…`) of the primary
    /// subscription item. Needed by the metered-usage reporter to
    /// POST `usage_records` for the customer. `None` when we've never
    /// seen a `customer.subscription.*` event carrying the item id
    /// (rows migrated in from schema v3 stay `None` until Stripe
    /// re-sends the next event, which is typically ≤ 24 h).
    pub subscription_item_id: Option<String>,
    /// RFC 3339 timestamp of the last update — for observability +
    /// operator triage. Set by the webhook handler when a subscription
    /// event is parsed (see `parse_subscription_object`) and passed
    /// through by the store verbatim, so tests can pin a deterministic
    /// value.
    pub updated_at: String,
}

/// Pending self-serve signup — a token was emailed to the applicant
/// but they haven't clicked it yet.
///
/// The plaintext token is never stored; instead its SHA-256 hex hash
/// lands here. Verify-side lookup hashes the caller-supplied token and
/// looks up by hash. This keeps a compromised backup from being able
/// to activate outstanding invitations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSignup {
    /// SHA-256 hex of the token that was mailed to `email`.
    pub token_hash: String,
    /// Where the magic link was sent. Also becomes the Stripe customer
    /// email on activation.
    pub email: String,
    /// Human-readable org name the applicant provided. Slugified on
    /// activation to derive the `OrgId`.
    pub org_name: String,
    /// RFC 3339 timestamp of creation — audit/observability only.
    pub created_at: String,
    /// RFC 3339 timestamp beyond which `take_pending_signup` refuses to
    /// return the row. Enforced by both the store lookup AND the periodic
    /// GC pass — belt-and-braces because a persistent SQLite file could
    /// otherwise accumulate stale rows if GC is skipped.
    pub expires_at: String,
}

/// Errors from [`BillingStore`] backends.
#[derive(Debug, thiserror::Error)]
pub enum BillingStoreError {
    #[error("billing store backend error: {0}")]
    Backend(String),
}

/// In-memory store — loses state on restart. Fine for tests + local
/// demos; catastrophic for prod SaaS. Use [`SqliteBillingStore`] there.
#[derive(Debug, Default)]
pub struct InMemoryBillingStore {
    /// `org → stripe_customer_id`
    map: Mutex<HashMap<OrgId, String>>,
    /// `stripe_customer_id → subscription state`
    subs: Mutex<HashMap<String, SubscriptionState>>,
    /// `token_hash → PendingSignup`. Populated by `POST /v1/signup/request`,
    /// consumed atomically by `POST /v1/signup/verify`.
    pending: Mutex<HashMap<String, PendingSignup>>,
}

impl BillingStore for InMemoryBillingStore {
    fn record_customer(&self, org: &OrgId, customer_id: &str) -> Result<(), BillingStoreError> {
        let mut guard = self.map.lock().expect("billing map poisoned");
        guard.insert(org.clone(), customer_id.to_string());
        Ok(())
    }
    fn get_customer(&self, org: &OrgId) -> Option<String> {
        let guard = self.map.lock().expect("billing map poisoned");
        guard.get(org).cloned()
    }
    fn record_subscription(
        &self,
        customer_id: &str,
        state: &SubscriptionState,
    ) -> Result<(), BillingStoreError> {
        // Match SqliteBillingStore's precondition: the customer must
        // exist (via `record_customer`) before its subscription can be
        // recorded. Otherwise tests that pass against InMemory would
        // silently drop state against SQLite in prod.
        let map = self.map.lock().expect("billing map poisoned");
        if !map.values().any(|cid| cid == customer_id) {
            return Err(BillingStoreError::Backend(format!(
                "record_subscription: no customer recorded for customer_id={customer_id:?}"
            )));
        }
        drop(map);
        let mut guard = self.subs.lock().expect("billing subs poisoned");
        guard.insert(customer_id.to_string(), state.clone());
        Ok(())
    }
    fn get_subscription(&self, customer_id: &str) -> Option<SubscriptionState> {
        let guard = self.subs.lock().expect("billing subs poisoned");
        guard.get(customer_id).cloned()
    }
    fn org_by_customer(&self, customer_id: &str) -> Option<OrgId> {
        let guard = self.map.lock().expect("billing map poisoned");
        guard
            .iter()
            .find(|(_, id)| id.as_str() == customer_id)
            .map(|(o, _)| o.clone())
    }
    fn record_pending_signup(&self, signup: &PendingSignup) -> Result<(), BillingStoreError> {
        let mut guard = self.pending.lock().expect("pending signups poisoned");
        // Invalidate any prior row for the same email so re-requesting a
        // signup replaces the old token instead of leaving two live at
        // once. Same shape SQLite enforces via a UNIQUE index.
        guard.retain(|_, s| s.email != signup.email);
        guard.insert(signup.token_hash.clone(), signup.clone());
        Ok(())
    }
    fn take_pending_signup(&self, token_hash: &str, now: &str) -> Option<PendingSignup> {
        let mut guard = self.pending.lock().expect("pending signups poisoned");
        let entry = guard.remove(token_hash)?;
        // Expired rows are treated as absent: don't return them and don't
        // put them back (the GC pass will pick them up if it runs first,
        // but if verify beats the sweeper we still remove them here).
        if entry.expires_at.as_str() < now {
            return None;
        }
        Some(entry)
    }
    fn peek_pending_signup(&self, token_hash: &str, now: &str) -> Option<PendingSignup> {
        let guard = self.pending.lock().expect("pending signups poisoned");
        let entry = guard.get(token_hash)?;
        if entry.expires_at.as_str() < now {
            return None;
        }
        Some(entry.clone())
    }
    fn delete_pending_signup(&self, token_hash: &str) -> Result<(), BillingStoreError> {
        let mut guard = self.pending.lock().expect("pending signups poisoned");
        guard.remove(token_hash);
        Ok(())
    }
    fn gc_expired_signups(&self, now: &str) -> Result<u64, BillingStoreError> {
        let mut guard = self.pending.lock().expect("pending signups poisoned");
        let before = guard.len();
        guard.retain(|_, s| s.expires_at.as_str() >= now);
        Ok((before - guard.len()) as u64)
    }
}

mod sqlite_billing_backend;
pub use sqlite_billing_backend::SqliteBillingStore;

pub mod usage_reporter;
pub use usage_reporter::{UsageReporterConfig, UsageReporterHandle};

// -------- Stripe client ------------------------------------------------

/// Thin reqwest wrapper for the two Stripe endpoints this crate needs.
///
/// `Debug` is implemented manually to avoid leaking `secret_key` into
/// logs / panic backtraces / structured tracing output. The public
/// derive would print the field verbatim.
pub struct StripeClient {
    http: reqwest::Client,
    secret_key: String,
    /// Base URL — production always uses `https://api.stripe.com`.
    /// Tests point this at a wiremock instance via
    /// [`StripeClient::with_base`].
    base_url: String,
}

impl std::fmt::Debug for StripeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StripeClient")
            .field("secret_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl StripeClient {
    pub fn new(secret_key: impl Into<String>) -> Self {
        Self::with_base(secret_key, "https://api.stripe.com")
    }

    /// Test-only base-URL override. Hidden from rustdoc so it can't
    /// creep into production call-sites.
    #[doc(hidden)]
    pub fn with_base(secret_key: impl Into<String>, base: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("build reqwest client"),
            secret_key: secret_key.into(),
            base_url: base.into(),
        }
    }

    /// Create a Stripe customer for a signup.
    ///
    /// `name` is the human-readable org name shown on Stripe invoices +
    /// dashboard. `org_slug` is the stable internal id used both as the
    /// nanovm `OrgId` and as `metadata[org_id]` so Stripe → nanovm
    /// reconciliation stays deterministic across `name` renames.
    ///
    /// `idempotency_key` is the value Stripe uses to deduplicate this
    /// request for 24 h. A network retry after Stripe already accepted
    /// the request (but our response was dropped) hits the cache and
    /// returns the existing customer instead of minting a duplicate.
    /// Callers must pick a key that's stable across retries but unique
    /// per logical signup — the magic-link `token_hash` is the natural
    /// choice for self-serve; the admin `signup` path can use the
    /// org_slug.
    pub async fn create_customer(
        &self,
        email: &str,
        name: &str,
        org_slug: &str,
        idempotency_key: &str,
    ) -> Result<StripeCustomer, BillingError> {
        let params = [
            ("email", email),
            ("name", name),
            ("metadata[org_id]", org_slug),
        ];
        let resp = self
            .http
            .post(format!("{}/v1/customers", self.base_url))
            .basic_auth(&self.secret_key, Some(""))
            .header("Idempotency-Key", idempotency_key)
            .form(&params)
            .send()
            .await
            .map_err(|e| BillingError::StripeTransport(e.to_string()))?;
        parse_stripe_response(resp).await
    }

    /// Mint a short-lived Stripe billing portal session.
    pub async fn create_billing_portal_session(
        &self,
        customer_id: &str,
        return_url: &str,
    ) -> Result<PortalSession, BillingError> {
        let params = [("customer", customer_id), ("return_url", return_url)];
        let resp = self
            .http
            .post(format!("{}/v1/billing_portal/sessions", self.base_url))
            .basic_auth(&self.secret_key, Some(""))
            .form(&params)
            .send()
            .await
            .map_err(|e| BillingError::StripeTransport(e.to_string()))?;
        parse_stripe_response(resp).await
    }

    /// Report metered usage for a subscription item.
    ///
    /// `subscription_item_id` is the `si_…` id captured from the
    /// primary item of the customer's subscription (see
    /// `parse_subscription_object`). `quantity` is the delta since
    /// the last report — Stripe adds it to the current billing
    /// period's meter with `action=increment`. `timestamp` is the
    /// caller's Unix epoch seconds; using a monotonic sequence
    /// prevents Stripe from silently coalescing reports.
    ///
    /// `idempotency_key` prevents a retry after a transient failure
    /// from being double-counted. The reporter derives the key from
    /// `(subscription_item_id, timestamp)` so a re-run with the same
    /// inputs is a no-op on Stripe.
    pub async fn report_usage_record(
        &self,
        subscription_item_id: &str,
        quantity: u64,
        timestamp: i64,
        idempotency_key: &str,
    ) -> Result<UsageRecord, BillingError> {
        let quantity_s = quantity.to_string();
        let ts_s = timestamp.to_string();
        let form = [
            ("quantity", quantity_s.as_str()),
            ("timestamp", ts_s.as_str()),
            ("action", "increment"),
        ];
        let resp = self
            .http
            .post(format!(
                "{}/v1/subscription_items/{subscription_item_id}/usage_records",
                self.base_url
            ))
            .basic_auth(&self.secret_key, Some(""))
            .header("Idempotency-Key", idempotency_key)
            .form(&form)
            .send()
            .await
            .map_err(|e| BillingError::StripeTransport(e.to_string()))?;
        parse_stripe_response(resp).await
    }
}

/// Stripe usage-record response. Only `id` is used today (for the
/// tracing log line); the rest of the fields Stripe returns are
/// ignored via serde's default behavior.
#[derive(Debug, Deserialize)]
pub struct UsageRecord {
    /// Stripe usage-record id (`mbur_…`).
    pub id: String,
}

/// Stripe customer object subset — just the fields we care about
/// (id + email). The full Stripe API response has many more fields;
/// `serde` ignores the rest.
#[derive(Debug, Deserialize)]
pub struct StripeCustomer {
    /// Stripe customer id, e.g. `cus_ABC123`. Never re-used by
    /// Stripe once assigned; safe to persist verbatim.
    pub id: String,
    /// Contact email echoed back from Stripe. `None` when the
    /// customer was created without one.
    #[serde(default)]
    pub email: Option<String>,
}

/// Stripe billing portal session — short-lived (~1 hour) URL the
/// customer opens to manage their subscription.
#[derive(Debug, Deserialize)]
pub struct PortalSession {
    /// Full Stripe portal URL. Redirect the customer here.
    pub url: String,
}

async fn parse_stripe_response<T: for<'de> Deserialize<'de>>(
    resp: reqwest::Response,
) -> Result<T, BillingError> {
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| BillingError::StripeTransport(e.to_string()))?;
    if status.is_success() {
        serde_json::from_slice::<T>(&bytes)
            .map_err(|e| BillingError::StripeTransport(e.to_string()))
    } else {
        let msg = extract_error_message(&bytes)
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
        Err(BillingError::StripeApi {
            status: status.as_u16(),
            message: msg,
        })
    }
}

fn extract_error_message(bytes: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Envelope {
        error: Inner,
    }
    #[derive(Deserialize)]
    struct Inner {
        message: String,
    }
    serde_json::from_slice::<Envelope>(bytes)
        .ok()
        .map(|e| e.error.message)
}

// -------- Email delivery ----------------------------------------------

/// How the control plane delivers signup magic links.
///
/// Prod deploys wire a real provider (Resend / Postmark / SES) via
/// [`ResendEmailSender`]. Local dev + self-hosted single-tenant boxes
/// use [`LogEmailSender`] — the magic link is written to the tracing
/// output at `info` so the operator can copy-paste it during testing.
pub trait EmailSender: Send + Sync + std::fmt::Debug {
    /// Deliver a magic-link email. `verify_url` is the fully-formed
    /// URL the recipient must open to activate the signup. Returns an
    /// error if the delivery could not be enqueued; a delivery that's
    /// silently dropped by the upstream (e.g. wrong `From` domain) is
    /// beyond this layer.
    fn send_magic_link<'a>(
        &'a self,
        to: &'a str,
        verify_url: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), EmailError>> + Send + 'a>>;
}

/// Delivery failures. `Transport` covers network/HTTP-layer errors,
/// `Provider` covers a 4xx/5xx from the upstream mail service with the
/// message they returned so ops can triage.
#[derive(Debug, thiserror::Error)]
pub enum EmailError {
    #[error("email transport error: {0}")]
    Transport(String),
    #[error("email provider error ({status}): {message}")]
    Provider { status: u16, message: String },
}

/// Development / self-hosted sender. Emits the magic link to
/// `tracing::info!` and considers that a successful send. **Never**
/// use in prod: the operator's log aggregator would leak the token to
/// anyone with log read.
#[derive(Debug, Default)]
pub struct LogEmailSender;

impl EmailSender for LogEmailSender {
    fn send_magic_link<'a>(
        &'a self,
        to: &'a str,
        verify_url: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), EmailError>> + Send + 'a>>
    {
        Box::pin(async move {
            tracing::info!(
                to,
                verify_url,
                "magic link (LogEmailSender — copy this into your browser)"
            );
            Ok(())
        })
    }
}

/// Production sender backed by Resend (`https://resend.com`). Chose
/// Resend for the smallest possible surface area (single JSON POST,
/// no OAuth, works from a distroless container against
/// `rustls-tls-webpki-roots`).
pub struct ResendEmailSender {
    http: reqwest::Client,
    api_key: String,
    /// `From:` header — must be a verified sender in the Resend dashboard.
    from: String,
}

impl std::fmt::Debug for ResendEmailSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResendEmailSender")
            .field("api_key", &"<redacted>")
            .field("from", &self.from)
            .finish()
    }
}

impl ResendEmailSender {
    pub fn new(api_key: impl Into<String>, from: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("build reqwest client"),
            api_key: api_key.into(),
            from: from.into(),
        }
    }
}

impl EmailSender for ResendEmailSender {
    fn send_magic_link<'a>(
        &'a self,
        to: &'a str,
        verify_url: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), EmailError>> + Send + 'a>>
    {
        Box::pin(async move {
            let subject = "Verify your nanovm signup";
            let html = format!(
                "<p>Someone (hopefully you) started a signup for this address at nanovm.</p>\
                 <p><a href=\"{verify_url}\">Click here to verify and finish setting up your account</a>.</p>\
                 <p>This link expires in 15 minutes. If you didn't request it, ignore this email.</p>"
            );
            let text = format!(
                "Someone (hopefully you) started a signup for this address at nanovm.\n\n\
                 Verify: {verify_url}\n\n\
                 This link expires in 15 minutes. If you didn't request it, ignore this email."
            );
            let body = serde_json::json!({
                "from": self.from,
                "to": [to],
                "subject": subject,
                "html": html,
                "text": text,
            });
            let resp = self
                .http
                .post("https://api.resend.com/emails")
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| EmailError::Transport(e.to_string()))?;
            let status = resp.status();
            if status.is_success() {
                Ok(())
            } else {
                let bytes = resp
                    .bytes()
                    .await
                    .map_err(|e| EmailError::Transport(e.to_string()))?;
                let message = extract_error_message(&bytes)
                    .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
                Err(EmailError::Provider {
                    status: status.as_u16(),
                    message,
                })
            }
        })
    }
}

// -------- Handler functions -------------------------------------------

/// Hash a magic-link token with SHA-256 → lowercase hex. Both sides
/// of the flow (record + take) go through here so the store never
/// touches the plaintext token.
pub(crate) fn hash_signup_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    let bytes = h.finalize();
    hex_encode(&bytes)
}

/// Generate a fresh 24-byte URL-safe token. Uses `rand::rngs::OsRng`
/// via `getrandom` (already in the reqwest transitive dep tree, no new
/// crate needed here — we call `getrandom` directly).
fn mint_signup_token() -> String {
    let mut buf = [0u8; 24];
    getrandom::getrandom(&mut buf).expect("getrandom is available on all supported platforms");
    // Base64-url without padding: 24 bytes → 32 chars, only [A-Za-z0-9_-].
    // Hand-rolled so we don't pull in a base64-url dep for this one use.
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(32);
    for chunk in buf.chunks(3) {
        let a = chunk[0];
        let b = chunk[1];
        let c = chunk[2];
        out.push(ALPHA[(a >> 2) as usize] as char);
        out.push(ALPHA[(((a & 0b11) << 4) | (b >> 4)) as usize] as char);
        out.push(ALPHA[(((b & 0b1111) << 2) | (c >> 6)) as usize] as char);
        out.push(ALPHA[(c & 0b111111) as usize] as char);
    }
    out
}

/// Self-serve signup step 1: create a pending signup and email the
/// magic link. Returns the same opaque response whether the address
/// is new, already-live, or malformed — this stops the endpoint from
/// leaking existence of a registered address (a common enumeration
/// vector on signup forms).
///
/// The token itself is never stored; only its SHA-256 hash lands in
/// `pending_signups`. The email carries the raw token, embedded in a
/// URL of the form `{verify_url_base}?token={raw}`.
pub async fn signup_request(
    store: &dyn BillingStore,
    email: &dyn EmailSender,
    verify_url_base: &str,
    ttl_secs: i64,
    req: SignupRequestRequest,
) -> Result<SignupRequestResponse, BillingError> {
    // Minimal validation. We don't reject on "email looks weird" —
    // the mail provider is authoritative, and rejecting narrowly here
    // would risk leaking which local-part shapes are accepted.
    let email_addr = req.email.trim().to_owned();
    let org_name = req.org.trim().to_owned();
    if !email_addr.contains('@') || org_name.is_empty() {
        // Return the SAME opaque response so the caller can't tell
        // what failed. Log at debug for ops.
        tracing::debug!("signup_request: rejecting malformed input (opaque response returned)");
        return Ok(SignupRequestResponse {
            message: "If that address is eligible, a verification link has been sent.".into(),
        });
    }

    let token = mint_signup_token();
    let token_hash = hash_signup_token(&token);
    let now = crate::time::rfc3339_now();
    let expires_at = crate::time::rfc3339_offset(ttl_secs);
    let pending = PendingSignup {
        token_hash,
        email: email_addr.clone(),
        org_name,
        created_at: now,
        expires_at,
    };
    // Persist BEFORE sending the email so a delivery failure doesn't
    // leave a phantom accepted signup the recipient can't verify. If
    // record fails we return an opaque OK too — the operator sees the
    // error in tracing, the user sees "check your inbox" (they'll
    // notice the missing email and retry).
    if let Err(e) = store.record_pending_signup(&pending) {
        tracing::error!(
            error = %e,
            email = %email_addr,
            "signup_request: failed to persist pending signup"
        );
        return Ok(SignupRequestResponse {
            message: "If that address is eligible, a verification link has been sent.".into(),
        });
    }
    let verify_url = format!("{verify_url_base}?token={token}");
    if let Err(e) = email.send_magic_link(&email_addr, &verify_url).await {
        tracing::error!(
            error = %e,
            email = %email_addr,
            "signup_request: failed to deliver magic link"
        );
    }
    Ok(SignupRequestResponse {
        message: "If that address is eligible, a verification link has been sent.".into(),
    })
}

/// Self-serve signup step 2: activate a pending signup using the token
/// from the magic link. Reuses the same "Stripe customer → persist →
/// mint API key" sequence as the admin `signup` path.
///
/// Ordering (audit-driven):
///   1. **Peek** the pending row — read-only lookup so a Stripe failure
///      doesn't leave the user with a consumed token AND no customer.
///   2. Call Stripe `create_customer` with `Idempotency-Key = token_hash`.
///      A network retry after Stripe accepted the request (but our
///      response dropped) hits the Stripe cache instead of creating a
///      duplicate `cus_…`.
///   3. Persist the org → customer_id mapping locally.
///   4. **Now** delete the pending row — the signup is committed.
///   5. Mint the API key (pure local op).
pub async fn signup_verify(
    tokens: &ApiTokens,
    store: &dyn BillingStore,
    stripe: &StripeClient,
    req: SignupVerifyRequest,
) -> Result<SignupResponse, BillingError> {
    let token_hash = hash_signup_token(&req.token);
    let now = crate::time::rfc3339_now();
    let pending = store
        .peek_pending_signup(&token_hash, &now)
        .ok_or(BillingError::InvalidSignupToken)?;
    let org_slug = slugify(&pending.org_name);
    if org_slug.is_empty() {
        return Err(BillingError::InvalidOrg);
    }
    let org_id = OrgId(org_slug.clone());
    let customer = stripe
        .create_customer(&pending.email, &pending.org_name, &org_slug, &token_hash)
        .await?;
    store.record_customer(&org_id, &customer.id)?;
    // Stripe accepted + local persistence succeeded — now consume the
    // pending row. If this delete fails the user has an activated
    // signup with a still-live token; next verify hits Stripe's
    // idempotency cache and returns the same customer, then re-runs
    // record_customer (an UPSERT), so it's idempotent end-to-end.
    if let Err(e) = store.delete_pending_signup(&token_hash) {
        tracing::warn!(
            error = %e,
            token_hash,
            "signup_verify: delete_pending_signup failed post-commit"
        );
    }
    let issued: IssuedToken = tokens.issue(org_id);
    Ok(SignupResponse {
        org: org_slug,
        api_key: issued.token,
        stripe_customer_id: customer.id,
    })
}

/// Signup semantic.
///
/// Steps: verify admin bearer → slugify org → **call Stripe** → mint
/// first API key → persist org → customer_id.
///
/// Order matters: Stripe runs first because it's the fallible external
/// call. If we minted + persisted the API key before Stripe and Stripe
/// then failed, the retry would collide with the still-live key and
/// the operator would have to hand-clean state. With Stripe first, a
/// failure returns the error to the caller and leaves state untouched
/// — retries are safe. The token mint is a pure local op; ordering it
/// after Stripe is essentially free.
pub async fn signup(
    cfg: &BillingConfig,
    tokens: &ApiTokens,
    store: &dyn BillingStore,
    stripe: &StripeClient,
    bearer: Option<&str>,
    req: SignupRequest,
) -> Result<SignupResponse, BillingError> {
    // Constant-time compare so the admin bearer isn't timing-side-channel
    // recoverable byte-by-byte. `==`/`!=` on `&str` short-circuits at
    // the first differing byte; over enough retries an attacker can
    // reconstruct the token. `subtle`-style compare avoids that.
    let bearer_ok = bearer
        .map(|b| constant_time_eq_bytes(b.as_bytes(), cfg.signup_token.as_bytes()))
        .unwrap_or(false);
    if !bearer_ok {
        return Err(BillingError::BadSignupToken);
    }

    let org_slug = slugify(&req.org);
    if org_slug.is_empty() {
        return Err(BillingError::InvalidOrg);
    }
    let org_id = OrgId(org_slug.clone());

    // 1) External call first — the only step that can fail with side
    //    effects landing outside our control (a customer object on
    //    Stripe). If it fails, we return before any local state
    //    mutation, so a retry is safe. Idempotency-Key = `org_slug` so
    //    a network-level retry doesn't create a duplicate cus_… on
    //    Stripe's side.
    let customer = stripe
        .create_customer(&req.email, &req.org, &org_slug, &org_slug)
        .await?;
    // 2) Persist the mapping BEFORE minting the token so a signup that
    //    crashes between these two steps loses the token, not the
    //    Stripe binding — the operator can reissue a token via the
    //    normal `/v1/keys` route, but a lost customer id is untraceable.
    store.record_customer(&org_id, &customer.id)?;
    // 3) Local mint. Can't fail; safe to run last.
    let issued: IssuedToken = tokens.issue(org_id);

    Ok(SignupResponse {
        org: org_slug,
        api_key: issued.token,
        stripe_customer_id: customer.id,
    })
}

/// Billing portal semantic — resolves the caller's org's Stripe
/// customer id and returns a portal session URL.
pub async fn billing_portal(
    cfg: &BillingConfig,
    store: &dyn BillingStore,
    stripe: &StripeClient,
    caller_org: &OrgId,
) -> Result<PortalResponse, BillingError> {
    let customer_id = store
        .get_customer(caller_org)
        .ok_or_else(|| BillingError::NoCustomerForOrg(caller_org.0.clone()))?;
    let session = stripe
        .create_billing_portal_session(&customer_id, &cfg.portal_return_url)
        .await?;
    Ok(PortalResponse { url: session.url })
}

// -------- Helpers ------------------------------------------------------

/// Slugify an org name: lowercases, keeps `[a-z0-9]`, replaces runs of
/// separators with a single `-`, trims trailing dashes.
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_dash = true;
    for ch in input.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

// -------- Webhook signature verification ------------------------------

/// Errors from webhook signature verification.
#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    #[error("webhook signing secret not configured")]
    NotConfigured,
    #[error("missing Stripe-Signature header")]
    MissingHeader,
    #[error("malformed Stripe-Signature header (expected `t=…,v1=…`)")]
    MalformedHeader,
    #[error("timestamp {ts} outside tolerance window (now={now}, tolerance={tolerance_secs}s)")]
    StaleTimestamp {
        ts: i64,
        now: i64,
        tolerance_secs: i64,
    },
    #[error("signature mismatch")]
    SignatureMismatch,
    #[error("body is not valid JSON: {0}")]
    BadJson(String),
}

/// Parse a `Stripe-Signature` header and return `(timestamp, v1_signatures)`.
///
/// Stripe's canonical form is `t=<unix_ts>,v1=<hex_sig>[,v1=<hex_sig>...]`.
/// Multiple `v1=` values may be present during signing-secret rotation —
/// we accept a valid match against any one.
fn parse_stripe_signature(header: &str) -> Result<(i64, Vec<String>), WebhookError> {
    let mut timestamp: Option<i64> = None;
    let mut v1s: Vec<String> = Vec::new();
    for part in header.split(',') {
        let (k, v) = part.split_once('=').ok_or(WebhookError::MalformedHeader)?;
        match k.trim() {
            "t" => {
                timestamp = Some(
                    v.trim()
                        .parse()
                        .map_err(|_| WebhookError::MalformedHeader)?,
                )
            }
            "v1" => v1s.push(v.trim().to_string()),
            // Ignore v0 + unknown schemes — Stripe reserves the right to add
            // new ones, and skipping them keeps forward-compat safe.
            _ => {}
        }
    }
    let ts = timestamp.ok_or(WebhookError::MalformedHeader)?;
    if v1s.is_empty() {
        return Err(WebhookError::MalformedHeader);
    }
    Ok((ts, v1s))
}

/// Constant-time comparison of two hex-encoded byte strings.
fn constant_time_eq_hex(a: &str, b: &str) -> bool {
    constant_time_eq_bytes(a.as_bytes(), b.as_bytes())
}

/// Byte-wise constant-time equality — length mismatch → false, otherwise
/// XORs every byte-pair and returns the OR-fold. Used for both webhook
/// signatures and admin-bearer comparison. Not "constant time" against
/// a length-based side channel; the length check IS a fast exit for
/// mismatched lengths, which is fine because a valid caller always
/// passes matching-length inputs.
fn constant_time_eq_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Verify a Stripe webhook payload against the `Stripe-Signature`
/// header per <https://stripe.com/docs/webhooks/signatures>.
///
/// - `header`: the raw `Stripe-Signature` header value
/// - `payload`: the raw HTTP body bytes (**do not** re-encode a
///   parsed JSON value — Stripe signs the exact bytes)
/// - `secret`: the `whsec_…` signing secret from the Stripe dashboard
/// - `now`: current unix time (parameter for testability)
/// - `tolerance_secs`: replay window — Stripe recommends 300 (5 min)
pub fn verify_webhook_signature(
    header: &str,
    payload: &[u8],
    secret: &str,
    now: i64,
    tolerance_secs: i64,
) -> Result<(), WebhookError> {
    use hmac::{Mac, SimpleHmac};
    use sha2::Sha256;

    let (ts, v1s) = parse_stripe_signature(header)?;
    // Use `abs_diff` so an attacker-controlled `t=` value near `i64::MIN`
    // can't overflow the subtraction and panic in overflow-checked builds.
    // `tolerance_secs` is bounded to a small positive number by the caller,
    // and any legitimate Stripe payload lands within seconds of `now`.
    let skew = now.abs_diff(ts);
    // Callers pass a small positive number (typically 300). Clamp
    // negatives to 0 so the comparison rejects everything, and use
    // a direct `as` cast — the `try_from` fallback the earlier code
    // carried was dead once the max(0) was in place.
    let tolerance = tolerance_secs.max(0) as u64;
    if skew > tolerance {
        return Err(WebhookError::StaleTimestamp {
            ts,
            now,
            tolerance_secs,
        });
    }
    let mut mac = <SimpleHmac<Sha256> as Mac>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(ts.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    let computed = hex_encode(&mac.finalize().into_bytes());
    if v1s.iter().any(|v1| constant_time_eq_hex(&computed, v1)) {
        Ok(())
    } else {
        Err(WebhookError::SignatureMismatch)
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        write!(&mut out, "{b:02x}").expect("String write cannot fail");
    }
    out
}

/// Stripe webhook event envelope — just the fields we route on for now.
/// The rest of the object graph is ignored by `serde`.
#[derive(Debug, Deserialize)]
pub struct StripeEvent {
    pub id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

// -------- Route wiring -------------------------------------------------

/// Sub-state carried on [`crate::AppState`] when the `billing` feature
/// is enabled at compile-time AND `BillingConfig::from_env()` returned
/// `Some` at startup. Access via [`crate::AppState::billing`].
#[derive(Clone)]
pub struct BillingCtx {
    pub config: BillingConfig,
    pub store: Arc<dyn BillingStore>,
    pub stripe: Arc<StripeClient>,
    /// Named plan tiers from `NANOVM_PLAN_TIERS`. Empty when unset;
    /// the plan endpoint then returns `plan: null` for every caller.
    pub tiers: PlanTiers,
    /// How magic-link emails go out. Defaults to [`LogEmailSender`]
    /// (dev/self-hosted); wire [`ResendEmailSender`] in prod by setting
    /// `RESEND_API_KEY` + `NANOVM_SIGNUP_FROM`.
    pub email: Arc<dyn EmailSender>,
}

impl std::fmt::Debug for BillingCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BillingCtx")
            .field("config", &self.config)
            .field("store", &"<dyn BillingStore>")
            .field("stripe", &"<StripeClient>")
            .finish()
    }
}

/// `POST /v1/signup` axum handler. Self-authenticates against
/// `NANOVM_SIGNUP_TOKEN` — the standard tenant auth middleware
/// deliberately does NOT run on this route, because the caller is an
/// operator provisioning a *new* org and by definition has no tenant
/// token yet.
pub(crate) async fn signup_handler(
    State(state): State<crate::AppState>,
    Extension(tokens): Extension<Arc<ApiTokens>>,
    headers: HeaderMap,
    body: Result<Json<SignupRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<SignupResponse>), ApiError> {
    let ctx = state.billing_ctx().ok_or(ApiError::Unsupported {
        code: "billing_disabled",
        message: "billing endpoints require the `billing` feature + \
                  STRIPE_SECRET_KEY, STRIPE_BILLING_PORTAL_RETURN_URL, \
                  NANOVM_SIGNUP_TOKEN env vars"
            .into(),
    })?;
    let Json(req) = body?;
    let bearer = extract_bearer(&headers);
    let resp = signup(
        &ctx.config,
        &tokens,
        ctx.store.as_ref(),
        &ctx.stripe,
        bearer.as_deref(),
        req,
    )
    .await
    .map_err(billing_to_api_error)?;
    Ok((StatusCode::CREATED, Json(resp)))
}

/// `POST /v1/signup/request` axum handler. Self-serve — no bearer.
/// Rate-limiting is expected UPSTREAM (reverse proxy / LB per-IP)
/// today; there is no in-process limiter on this route. Returns the
/// same opaque body regardless of outcome so callers can't enumerate
/// live email addresses; the operator sees the real story in tracing.
pub(crate) async fn signup_request_handler(
    State(state): State<crate::AppState>,
    body: Result<Json<SignupRequestRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<SignupRequestResponse>), ApiError> {
    let ctx = state.billing_ctx().ok_or(ApiError::Unsupported {
        code: "billing_disabled",
        message: "billing endpoints require the `billing` feature + Stripe env vars".into(),
    })?;
    // Body-parse errors get the opaque response too — same rationale
    // as the email-doesn't-exist path.
    let Ok(Json(req)) = body else {
        return Ok((
            StatusCode::ACCEPTED,
            Json(SignupRequestResponse {
                message: "If that address is eligible, a verification link has been sent.".into(),
            }),
        ));
    };
    let resp = signup_request(
        ctx.store.as_ref(),
        ctx.email.as_ref(),
        &ctx.config.signup_verify_url,
        ctx.config.signup_token_ttl_secs,
        req,
    )
    .await
    .map_err(billing_to_api_error)?;
    Ok((StatusCode::ACCEPTED, Json(resp)))
}

/// `POST /v1/signup/verify` axum handler. Consumes the magic-link
/// token, activates the pending signup, returns the first API key.
pub(crate) async fn signup_verify_handler(
    State(state): State<crate::AppState>,
    Extension(tokens): Extension<Arc<ApiTokens>>,
    body: Result<Json<SignupVerifyRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<SignupResponse>), ApiError> {
    let ctx = state.billing_ctx().ok_or(ApiError::Unsupported {
        code: "billing_disabled",
        message: "billing endpoints require the `billing` feature + Stripe env vars".into(),
    })?;
    let Json(req) = body?;
    let resp = signup_verify(&tokens, ctx.store.as_ref(), &ctx.stripe, req)
        .await
        .map_err(billing_to_api_error)?;
    Ok((StatusCode::CREATED, Json(resp)))
}

/// `GET /v1/billing/portal` axum handler. Reaches this handler ONLY
/// after `auth::require_token` middleware injected the caller's
/// [`OrgId`], so `Extension<OrgId>` is always present here.
pub(crate) async fn billing_portal_handler(
    State(state): State<crate::AppState>,
    Extension(org): Extension<OrgId>,
) -> Result<Json<PortalResponse>, ApiError> {
    let ctx = state.billing_ctx().ok_or(ApiError::Unsupported {
        code: "billing_disabled",
        message: "billing endpoints require the `billing` feature + Stripe env vars".into(),
    })?;
    let resp = billing_portal(&ctx.config, ctx.store.as_ref(), &ctx.stripe, &org)
        .await
        .map_err(billing_to_api_error)?;
    Ok(Json(resp))
}

/// `GET /v1/billing/plan` — return the resolved plan for the caller's
/// org. Tenant-authenticated: the caller's `OrgId` is injected by the
/// standard token middleware. Cheap: three in-memory / SQLite lookups,
/// no external calls.
pub(crate) async fn plan_handler(
    State(state): State<crate::AppState>,
    Extension(org): Extension<OrgId>,
) -> Result<Json<PlanResponse>, ApiError> {
    let ctx = state.billing_ctx().ok_or(ApiError::Unsupported {
        code: "billing_disabled",
        message: "billing endpoints require the `billing` feature + Stripe env vars".into(),
    })?;
    Ok(Json(resolve_plan(&ctx.tiers, ctx.store.as_ref(), &org)))
}

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    raw.strip_prefix("Bearer ").map(str::to_string)
}

/// `POST /v1/stripe/webhook` — verify signature, log the event,
/// return 200. Downstream event routing (subscription tier changes,
/// customer.deleted, invoice.paid) lives in a follow-up PR; this one
/// gets the signature-verification wall standing.
pub(crate) async fn stripe_webhook_handler(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<StatusCode, ApiError> {
    let ctx = state.billing_ctx().ok_or(ApiError::Unsupported {
        code: "billing_disabled",
        message: "billing endpoints require the `billing` feature + Stripe env vars".into(),
    })?;
    let secret = ctx
        .config
        .webhook_signing_secret
        .as_ref()
        .ok_or(ApiError::Unsupported {
            code: "webhook_disabled",
            message: "STRIPE_WEBHOOK_SIGNING_SECRET not configured".into(),
        })?;
    let header = headers
        .get("stripe-signature")
        .ok_or_else(|| ApiError::Bad("missing Stripe-Signature header".into()))?
        .to_str()
        .map_err(|_| ApiError::Bad("Stripe-Signature is not valid UTF-8".into()))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    verify_webhook_signature(header, &body, secret, now, 300).map_err(webhook_to_api_error)?;
    let event: StripeEvent = serde_json::from_slice(&body)
        .map_err(|e| ApiError::Bad(format!("webhook body is not valid JSON: {e}")))?;
    tracing::info!(
        event_id = %event.id,
        event_type = %event.event_type,
        "stripe webhook received"
    );
    // Bump the observability counter before routing so unknown event
    // types (which have no side effect) still show up in `/metrics`.
    // Cardinality is bounded by Stripe's own event-type enum.
    state.metrics().record_stripe_event(&event.event_type);
    // Route the event. Unknown event types return 200 (Stripe treats
    // 2xx as "delivered"; returning 5xx would cause retries), but we
    // log them so an operator can see what's arriving.
    route_webhook_event(ctx.store.as_ref(), &event);
    Ok(StatusCode::OK)
}

/// Route a `StripeEvent` to its side effect. Persists subscription
/// state on `customer.subscription.*`, and structured-logs invoice
/// lifecycle events (`invoice.paid`, `invoice.payment_failed`) so ops
/// dashboards can surface payment-flow health without waiting for the
/// downstream `customer.subscription.updated` that Stripe sends after.
///
/// Everything else returns without side effect (2xx to Stripe; a 5xx
/// would provoke retries) but the outer handler still records a
/// per-event-type counter for observability.
pub(crate) fn route_webhook_event(store: &dyn BillingStore, event: &StripeEvent) {
    match event.event_type.as_str() {
        "customer.subscription.created"
        | "customer.subscription.updated"
        | "customer.subscription.deleted" => match parse_subscription_object(event) {
            Ok((customer_id, state)) => {
                if let Err(e) = store.record_subscription(&customer_id, &state) {
                    tracing::error!(
                        error = %e,
                        customer_id,
                        "webhook: record_subscription failed"
                    );
                } else {
                    tracing::info!(
                        customer_id,
                        subscription_id = %state.subscription_id,
                        status = %state.status,
                        price_id = ?state.price_id,
                        event_type = %event.event_type,
                        "webhook: recorded subscription state"
                    );
                }
            }
            Err(e) => tracing::warn!(
                error = %e,
                event_id = %event.id,
                event_type = %event.event_type,
                "webhook: could not parse subscription object"
            ),
        },
        "invoice.paid" => match parse_invoice_object(event) {
            Ok(inv) => tracing::info!(
                event_id = %event.id,
                customer_id = %inv.customer_id,
                invoice_id = %inv.invoice_id,
                amount_paid = inv.amount_paid,
                hosted_invoice_url = ?inv.hosted_invoice_url,
                "webhook: invoice paid"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                event_id = %event.id,
                "webhook: could not parse invoice.paid object"
            ),
        },
        "invoice.payment_failed" => match parse_invoice_object(event) {
            Ok(inv) => tracing::warn!(
                event_id = %event.id,
                customer_id = %inv.customer_id,
                invoice_id = %inv.invoice_id,
                amount_due = inv.amount_due,
                hosted_invoice_url = ?inv.hosted_invoice_url,
                "webhook: invoice payment failed"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                event_id = %event.id,
                "webhook: could not parse invoice.payment_failed object"
            ),
        },
        other => tracing::debug!(event_type = other, "webhook: no handler, ignored"),
    }
}

/// Fields we care about from a Stripe `invoice` object.
#[derive(Debug, PartialEq, Eq)]
struct InvoiceObject {
    invoice_id: String,
    customer_id: String,
    /// Cents (Stripe's smallest currency unit). `amount_paid` on
    /// invoice.paid, may still be 0 on invoice.payment_failed.
    amount_paid: i64,
    /// Cents. Non-zero on invoice.payment_failed indicates what the
    /// customer owes.
    amount_due: i64,
    /// Stripe's hosted receipt/pay-invoice URL — the operator can
    /// hand this to the customer when triaging a failed payment.
    hosted_invoice_url: Option<String>,
}

/// Extract the invoice fields we log from an `invoice.*` event's
/// `data.object`. Only `customer` is required; the rest fall back to
/// sensible defaults so a partial payload still yields a log line.
fn parse_invoice_object(event: &StripeEvent) -> Result<InvoiceObject, &'static str> {
    let data = event.data.as_ref().ok_or("event has no data")?;
    let object = data.get("object").ok_or("data.object missing")?;
    let customer_id = object
        .get("customer")
        .and_then(|v| v.as_str())
        .ok_or("data.object.customer missing")?
        .to_string();
    let invoice_id = object
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("<none>")
        .to_string();
    let amount_paid = object
        .get("amount_paid")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let amount_due = object
        .get("amount_due")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let hosted_invoice_url = object
        .get("hosted_invoice_url")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(InvoiceObject {
        invoice_id,
        customer_id,
        amount_paid,
        amount_due,
        hosted_invoice_url,
    })
}

/// Extract `(customer_id, SubscriptionState)` from a
/// `customer.subscription.*` event's `data.object`. Stripe's event
/// envelope wraps the subscription at `event.data.object`; the fields
/// we care about are `id`, `customer`, `status`, and the primary
/// item's price id at `items.data[0].price.id`.
fn parse_subscription_object(
    event: &StripeEvent,
) -> Result<(String, SubscriptionState), &'static str> {
    let data = event.data.as_ref().ok_or("event has no data")?;
    let object = data.get("object").ok_or("data.object missing")?;
    let subscription_id = object
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("data.object.id missing")?
        .to_string();
    let customer_id = object
        .get("customer")
        .and_then(|v| v.as_str())
        .ok_or("data.object.customer missing")?
        .to_string();
    let status = object
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or("data.object.status missing")?
        .to_string();
    // Primary subscription item. Stripe multi-item subscriptions do
    // exist; we pick the first one which matches the common
    // single-item shape most SaaS deploys use. A follow-up can extend
    // this to enumerate multi-item subscriptions.
    //
    // We capture both:
    //   `items.data[0].price.id` — used to map to the named tier
    //     configured in `NANOVM_PLAN_TIERS`.
    //   `items.data[0].id` — the SUBSCRIPTION_ITEM id (`si_…`), needed
    //     by the metered reporter to POST usage_records.
    let primary_item = object
        .get("items")
        .and_then(|v| v.get("data"))
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first());
    let price_id = primary_item
        .and_then(|item| item.get("price"))
        .and_then(|price| price.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let subscription_item_id = primary_item
        .and_then(|item| item.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok((
        customer_id,
        SubscriptionState {
            subscription_id,
            status,
            price_id,
            subscription_item_id,
            updated_at: crate::time::rfc3339_now(),
        },
    ))
}

fn webhook_to_api_error(e: WebhookError) -> ApiError {
    match e {
        WebhookError::NotConfigured => ApiError::Unsupported {
            code: "webhook_disabled",
            message: "STRIPE_WEBHOOK_SIGNING_SECRET not configured".into(),
        },
        WebhookError::MissingHeader
        | WebhookError::MalformedHeader
        | WebhookError::StaleTimestamp { .. }
        | WebhookError::SignatureMismatch
        | WebhookError::BadJson(_) => ApiError::Forbidden {
            code: "invalid_signature",
            message: e.to_string(),
        },
    }
}

fn billing_to_api_error(e: BillingError) -> ApiError {
    match e {
        BillingError::Disabled => ApiError::Unsupported {
            code: "billing_disabled",
            message: "billing endpoints not configured".into(),
        },
        BillingError::BadSignupToken => {
            ApiError::Unauthorized("signup requires NANOVM_SIGNUP_TOKEN as the bearer".into())
        }
        BillingError::InvalidOrg => {
            ApiError::Bad("org name must contain at least one alphanumeric character".into())
        }
        BillingError::NoCustomerForOrg(org) => ApiError::NotFound {
            code: "no_billing_customer",
            message: format!("no Stripe customer recorded for org {org:?}; did you signup?"),
        },
        BillingError::StripeApi { status, message } => {
            // Never echo Stripe's raw message to the caller — it can
            // name internal Stripe object IDs, the caller's email,
            // "no such price price_XXX", etc. Log at `warn` so ops
            // sees the real story; return a generic body.
            tracing::warn!(
                stripe_status = status,
                stripe_message = %message,
                "billing: upstream Stripe error"
            );
            match status {
                429 => ApiError::TooManyRequests {
                    code: "billing_upstream_throttled",
                    message: "Stripe rate-limited the request; retry shortly.".into(),
                    // Stripe's own Retry-After header (which we don't parse
                    // yet) would be more accurate. 60s is a safe conservative
                    // default that matches Stripe's typical guidance.
                    retry_after_secs: 60,
                },
                500..=599 => ApiError::InternalDyn(
                    "upstream billing provider is unavailable; try again shortly.".into(),
                ),
                _ => ApiError::Bad(
                    "billing request was rejected by the upstream provider.".into(),
                ),
            }
        }
        BillingError::StripeTransport(msg) => {
            tracing::warn!(error = %msg, "billing: Stripe transport error");
            ApiError::InternalDyn("upstream billing provider is unreachable.".into())
        }
        BillingError::Store(inner) => ApiError::InternalDyn(inner.to_string()),
        BillingError::InvalidSignupToken => ApiError::Bad(
            "magic-link token is unknown or expired; request a fresh one via POST /v1/signup/request"
                .into(),
        ),
    }
}

// -------- Tests --------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Sign a payload the same way Stripe would — helper for the tests
    /// below. Returns the `t=…,v1=…` header value.
    fn sign(secret: &str, payload: &[u8], ts: i64) -> String {
        use hmac::{Mac, SimpleHmac};
        use sha2::Sha256;
        let mut mac = <SimpleHmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(ts.to_string().as_bytes());
        mac.update(b".");
        mac.update(payload);
        let sig = hex_encode(&mac.finalize().into_bytes());
        format!("t={ts},v1={sig}")
    }

    #[test]
    fn verify_accepts_a_correctly_signed_payload() {
        let body = br#"{"id":"evt_1","type":"customer.subscription.updated"}"#;
        let ts = 1_700_000_000;
        let header = sign("whsec_hush", body, ts);
        assert!(verify_webhook_signature(&header, body, "whsec_hush", ts, 300).is_ok());
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let body = br#"{"id":"evt_1"}"#;
        let ts = 1_700_000_000;
        let header = sign("whsec_hush", body, ts);
        let err = verify_webhook_signature(&header, body, "whsec_WRONG", ts, 300).unwrap_err();
        assert!(matches!(err, WebhookError::SignatureMismatch));
    }

    #[test]
    fn verify_rejects_mutated_payload() {
        let body = br#"{"id":"evt_1"}"#;
        let ts = 1_700_000_000;
        let header = sign("whsec_hush", body, ts);
        let err =
            verify_webhook_signature(&header, br#"{"id":"evt_TAMPERED"}"#, "whsec_hush", ts, 300)
                .unwrap_err();
        assert!(matches!(err, WebhookError::SignatureMismatch));
    }

    #[test]
    fn verify_rejects_stale_timestamp() {
        let body = br#"{"id":"evt_1"}"#;
        let ts = 1_700_000_000;
        let header = sign("whsec_hush", body, ts);
        // now is 301 s after the signing timestamp → outside tolerance.
        let err = verify_webhook_signature(&header, body, "whsec_hush", ts + 301, 300).unwrap_err();
        assert!(matches!(err, WebhookError::StaleTimestamp { .. }));
    }

    #[test]
    fn verify_rejects_stale_timestamp_from_the_future_too() {
        let body = br#"{}"#;
        let ts = 1_700_000_000;
        let header = sign("whsec_hush", body, ts);
        // A `now` 400 s BEFORE the timestamp is equally suspicious —
        // Stripe rounds server clocks but a caller clock 5+ min ahead
        // suggests a replay against a rewound host clock.
        let err = verify_webhook_signature(&header, body, "whsec_hush", ts - 400, 300).unwrap_err();
        assert!(matches!(err, WebhookError::StaleTimestamp { .. }));
    }

    #[test]
    fn verify_rejects_malformed_header() {
        let body = b"{}";
        let cases = [
            "",                   // empty
            "t=",                 // no timestamp value
            "v1=abc",             // no timestamp key
            "t=abc,v1=def",       // timestamp not an int
            "t=1700000000",       // no v1
            "t=1700000000,x=1",   // no v1 (unknown scheme)
            "t=1700000000v1=abc", // missing `=`
        ];
        for c in cases {
            let err =
                verify_webhook_signature(c, body, "whsec_hush", 1_700_000_000, 300).unwrap_err();
            assert!(
                matches!(err, WebhookError::MalformedHeader),
                "case {c:?} should return MalformedHeader, got {err:?}"
            );
        }
    }

    #[test]
    fn verify_accepts_any_valid_v1_when_multiple_present() {
        // During signing-key rotation Stripe emits multiple v1= values
        // signed with different secrets. We must accept if *any* match.
        let body = br#"{"id":"evt_r"}"#;
        let ts = 1_700_000_000;
        let good = sign("whsec_new", body, ts);
        // Strip the `t=…,` prefix off the "old-secret" signature so we
        // can inject it as a second v1= entry alongside the good one.
        let bad_only = sign("whsec_old", body, ts);
        let bad_v1 = bad_only.split(",v1=").nth(1).unwrap();
        let header = format!("{good},v1={bad_v1}");
        assert!(verify_webhook_signature(&header, body, "whsec_new", ts, 300).is_ok());
    }

    #[test]
    fn parse_signature_ignores_unknown_scheme_values() {
        let (ts, v1s) = parse_stripe_signature("t=1234,v0=deprecated,v1=abc,v2=future").unwrap();
        assert_eq!(ts, 1234);
        assert_eq!(v1s, vec!["abc".to_string()]);
    }

    #[test]
    fn constant_time_eq_hex_rejects_length_mismatch() {
        assert!(!constant_time_eq_hex("abc", "abcd"));
        assert!(constant_time_eq_hex("abc", "abc"));
        assert!(!constant_time_eq_hex("abc", "abd"));
    }

    #[test]
    fn parse_subscription_extracts_customer_status_and_price() {
        let event = StripeEvent {
            id: "evt_1".into(),
            event_type: "customer.subscription.updated".into(),
            data: Some(serde_json::json!({
                "object": {
                    "id": "sub_ABC",
                    "customer": "cus_ACME",
                    "status": "active",
                    "items": {
                        "data": [
                            {"price": {"id": "price_PRO"}}
                        ]
                    }
                }
            })),
        };
        let (cid, state) = parse_subscription_object(&event).unwrap();
        assert_eq!(cid, "cus_ACME");
        assert_eq!(state.subscription_id, "sub_ABC");
        assert_eq!(state.status, "active");
        assert_eq!(state.price_id.as_deref(), Some("price_PRO"));
    }

    #[test]
    fn parse_subscription_missing_price_is_ok_returns_none() {
        let event = StripeEvent {
            id: "evt_1".into(),
            event_type: "customer.subscription.deleted".into(),
            data: Some(serde_json::json!({
                "object": {
                    "id": "sub_ABC",
                    "customer": "cus_ACME",
                    "status": "canceled",
                }
            })),
        };
        let (_, state) = parse_subscription_object(&event).unwrap();
        assert_eq!(state.status, "canceled");
        assert!(state.price_id.is_none());
    }

    #[test]
    fn parse_subscription_rejects_missing_required_fields() {
        for missing in ["id", "customer", "status"] {
            let mut obj = serde_json::json!({
                "id": "sub_X",
                "customer": "cus_X",
                "status": "active",
            });
            obj.as_object_mut().unwrap().remove(missing);
            let event = StripeEvent {
                id: "evt".into(),
                event_type: "customer.subscription.updated".into(),
                data: Some(serde_json::json!({ "object": obj })),
            };
            assert!(
                parse_subscription_object(&event).is_err(),
                "missing {missing} should fail parse"
            );
        }
    }

    #[test]
    fn route_persists_subscription_and_updates_org_lookup() {
        let store = InMemoryBillingStore::default();
        store
            .record_customer(&OrgId::new("acme"), "cus_ACME")
            .unwrap();
        assert!(store.get_subscription("cus_ACME").is_none());

        let event = StripeEvent {
            id: "evt_1".into(),
            event_type: "customer.subscription.updated".into(),
            data: Some(serde_json::json!({
                "object": {
                    "id": "sub_A", "customer": "cus_ACME",
                    "status": "active",
                    "items": {"data":[{"price":{"id":"price_pro"}}]}
                }
            })),
        };
        route_webhook_event(&store, &event);

        let s = store.get_subscription("cus_ACME").expect("recorded");
        assert_eq!(s.status, "active");
        assert_eq!(s.price_id.as_deref(), Some("price_pro"));
        // Reverse lookup by customer id still works.
        assert_eq!(store.org_by_customer("cus_ACME"), Some(OrgId::new("acme")));
    }

    #[test]
    fn route_ignores_unknown_event_types() {
        let store = InMemoryBillingStore::default();
        let event = StripeEvent {
            id: "evt_1".into(),
            event_type: "customer.updated".into(),
            data: None,
        };
        route_webhook_event(&store, &event); // must not panic; also no-op
        assert!(store.get_subscription("cus_ANY").is_none());
    }

    #[test]
    fn route_deleted_persists_canceled_status() {
        // customer.subscription.deleted arrives with status="canceled" —
        // we persist it so the plan resolver / dashboard can distinguish
        // "canceled" from "never subscribed".
        let store = InMemoryBillingStore::default();
        store
            .record_customer(&OrgId::new("acme"), "cus_ACME")
            .unwrap();
        let event = StripeEvent {
            id: "evt_del".into(),
            event_type: "customer.subscription.deleted".into(),
            data: Some(serde_json::json!({
                "object": {
                    "id": "sub_A", "customer": "cus_ACME",
                    "status": "canceled",
                }
            })),
        };
        route_webhook_event(&store, &event);
        let s = store.get_subscription("cus_ACME").expect("recorded");
        assert_eq!(s.status, "canceled");
        assert!(s.price_id.is_none());
    }

    #[test]
    fn parse_invoice_extracts_customer_and_amounts() {
        let event = StripeEvent {
            id: "evt_p".into(),
            event_type: "invoice.paid".into(),
            data: Some(serde_json::json!({
                "object": {
                    "id": "in_ABC",
                    "customer": "cus_ACME",
                    "amount_paid": 1000,
                    "amount_due": 0,
                    "hosted_invoice_url": "https://stripe.example/i/abc",
                }
            })),
        };
        let inv = parse_invoice_object(&event).unwrap();
        assert_eq!(inv.invoice_id, "in_ABC");
        assert_eq!(inv.customer_id, "cus_ACME");
        assert_eq!(inv.amount_paid, 1000);
        assert_eq!(inv.amount_due, 0);
        assert_eq!(
            inv.hosted_invoice_url.as_deref(),
            Some("https://stripe.example/i/abc")
        );
    }

    #[test]
    fn parse_invoice_missing_optionals_falls_back_to_defaults() {
        // Real prod payloads may omit amounts / hosted url on
        // certain edge cases (e.g. $0 trials). Only `customer` is
        // required; the rest have sensible defaults.
        let event = StripeEvent {
            id: "evt_p".into(),
            event_type: "invoice.payment_failed".into(),
            data: Some(serde_json::json!({
                "object": { "customer": "cus_ACME" }
            })),
        };
        let inv = parse_invoice_object(&event).unwrap();
        assert_eq!(inv.customer_id, "cus_ACME");
        assert_eq!(inv.invoice_id, "<none>");
        assert_eq!(inv.amount_paid, 0);
        assert_eq!(inv.amount_due, 0);
        assert!(inv.hosted_invoice_url.is_none());
    }

    #[test]
    fn parse_invoice_missing_customer_errors() {
        let event = StripeEvent {
            id: "evt_p".into(),
            event_type: "invoice.paid".into(),
            data: Some(serde_json::json!({ "object": { "id": "in_x" } })),
        };
        assert!(parse_invoice_object(&event).is_err());
    }

    #[test]
    fn route_invoice_paid_does_not_touch_subscription_state() {
        // invoice.paid logs but must not mutate stored subscription
        // state — the paired customer.subscription.updated event
        // Stripe sends after is the authoritative signal.
        let store = InMemoryBillingStore::default();
        store
            .record_customer(&OrgId::new("acme"), "cus_ACME")
            .unwrap();
        store
            .record_subscription(
                "cus_ACME",
                &SubscriptionState {
                    subscription_id: "sub_A".into(),
                    status: "past_due".into(),
                    price_id: Some("price_pro".into()),
                    subscription_item_id: None,
                    updated_at: "2026-07-10T00:00:00Z".into(),
                },
            )
            .unwrap();

        let event = StripeEvent {
            id: "evt_p".into(),
            event_type: "invoice.paid".into(),
            data: Some(serde_json::json!({
                "object": { "id": "in_A", "customer": "cus_ACME",
                            "amount_paid": 1000 }
            })),
        };
        route_webhook_event(&store, &event);

        // Untouched.
        let s = store.get_subscription("cus_ACME").expect("still there");
        assert_eq!(s.status, "past_due");
    }

    #[test]
    fn route_invoice_paid_with_missing_data_does_not_panic() {
        let store = InMemoryBillingStore::default();
        let event = StripeEvent {
            id: "evt_p".into(),
            event_type: "invoice.paid".into(),
            data: None, // handler must warn-log and continue
        };
        route_webhook_event(&store, &event);
    }

    #[test]
    fn stripe_event_parses_minimum_fields() {
        let raw = br#"{"id":"evt_x","type":"customer.subscription.updated","data":{"object":{}}}"#;
        let e: StripeEvent = serde_json::from_slice(raw).unwrap();
        assert_eq!(e.id, "evt_x");
        assert_eq!(e.event_type, "customer.subscription.updated");
        assert!(e.data.is_some());
    }

    // ---- PlanTiers -----------------------------------------------

    #[test]
    fn plan_tiers_empty_when_env_unset() {
        let t = PlanTiers::parse("");
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn plan_tiers_parse_happy_path() {
        let t = PlanTiers::parse("price_free=free:5,price_pro=pro:100,price_ent=enterprise:1000");
        assert_eq!(t.len(), 3);
        assert_eq!(t.get("price_pro").unwrap().name, "pro");
        assert_eq!(t.get("price_pro").unwrap().rps, 100);
        assert!(t.get("price_ent").is_some());
    }

    #[test]
    fn plan_tiers_skip_malformed_entries_but_keep_valid_ones() {
        let t = PlanTiers::parse(
            "price_ok=pro:50,broken_no_equals,price_bad=missing_rps,,price_good=free:5",
        );
        assert_eq!(t.len(), 2);
        assert!(t.get("price_ok").is_some());
        assert!(t.get("price_good").is_some());
        assert!(t.get("broken_no_equals").is_none());
        assert!(t.get("price_bad").is_none());
    }

    #[test]
    fn plan_tiers_trims_whitespace() {
        let t = PlanTiers::parse(" price_x = pro : 42 , price_y = free : 1 ");
        assert_eq!(t.len(), 2);
        assert_eq!(t.get("price_x").unwrap().name, "pro");
        assert_eq!(t.get("price_x").unwrap().rps, 42);
    }

    // ---- resolve_plan ---------------------------------------------

    #[test]
    fn resolve_plan_returns_none_when_org_has_no_customer() {
        let store = InMemoryBillingStore::default();
        let tiers = PlanTiers::parse("price_pro=pro:100");
        let r = resolve_plan(&tiers, &store, &OrgId::new("nobody"));
        assert!(r.plan.is_none());
        assert!(r.subscription_status.is_none());
        assert!(r.price_id.is_none());
    }

    #[test]
    fn resolve_plan_returns_none_when_customer_has_no_subscription() {
        let store = InMemoryBillingStore::default();
        store
            .record_customer(&OrgId::new("acme"), "cus_ACME")
            .unwrap();
        let tiers = PlanTiers::parse("price_pro=pro:100");
        let r = resolve_plan(&tiers, &store, &OrgId::new("acme"));
        assert!(r.plan.is_none());
        assert!(r.subscription_status.is_none());
    }

    #[test]
    fn resolve_plan_maps_price_id_to_named_tier() {
        let store = InMemoryBillingStore::default();
        store.record_customer(&OrgId::new("acme"), "cus_A").unwrap();
        store
            .record_subscription(
                "cus_A",
                &SubscriptionState {
                    subscription_id: "sub_1".into(),
                    status: "active".into(),
                    price_id: Some("price_pro".into()),
                    subscription_item_id: None,
                    updated_at: "2026-07-10T00:00:00Z".into(),
                },
            )
            .unwrap();
        let tiers = PlanTiers::parse("price_pro=pro:100");
        let r = resolve_plan(&tiers, &store, &OrgId::new("acme"));
        assert_eq!(r.plan.as_ref().unwrap().name, "pro");
        assert_eq!(r.plan.as_ref().unwrap().rps, 100);
        assert_eq!(r.subscription_status.as_deref(), Some("active"));
        assert_eq!(r.price_id.as_deref(), Some("price_pro"));
    }

    #[test]
    fn resolve_plan_returns_subscription_but_null_tier_when_price_id_unknown() {
        // Operator forgot to update NANOVM_PLAN_TIERS after adding a
        // new Stripe price. The plan endpoint still reports the raw
        // subscription state so the dashboard can render "Unknown
        // plan (contact support)" instead of misreporting free.
        let store = InMemoryBillingStore::default();
        store.record_customer(&OrgId::new("acme"), "cus_A").unwrap();
        store
            .record_subscription(
                "cus_A",
                &SubscriptionState {
                    subscription_id: "sub_1".into(),
                    status: "active".into(),
                    price_id: Some("price_UNMAPPED".into()),
                    subscription_item_id: None,
                    updated_at: "2026-07-10T00:00:00Z".into(),
                },
            )
            .unwrap();
        let tiers = PlanTiers::parse("price_pro=pro:100");
        let r = resolve_plan(&tiers, &store, &OrgId::new("acme"));
        assert!(r.plan.is_none());
        assert_eq!(r.subscription_status.as_deref(), Some("active"));
        assert_eq!(r.price_id.as_deref(), Some("price_UNMAPPED"));
    }

    #[test]
    fn slugify_collapses_separators_and_trims_dashes() {
        assert_eq!(slugify("Acme Inc."), "acme-inc");
        assert_eq!(slugify("--Globex--Corporation--"), "globex-corporation");
        assert_eq!(slugify("42_Wallaby Way"), "42-wallaby-way");
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn billing_config_debug_redacts_secrets() {
        let cfg = BillingConfig {
            stripe_secret_key: "sk_test_ohno".into(),
            portal_return_url: "http://ok".into(),
            signup_token: "hush".into(),
            webhook_signing_secret: None,
            signup_verify_url: "http://localhost:8080/v1/signup/verify".into(),
            signup_token_ttl_secs: 900,
        };
        let s = format!("{cfg:?}");
        assert!(!s.contains("sk_test_ohno"));
        assert!(!s.contains("hush"));
        assert!(s.contains("<redacted>"));
        assert!(s.contains("http://ok"));
    }

    #[test]
    fn in_memory_billing_store_records_and_reads() {
        let store = InMemoryBillingStore::default();
        let acme = OrgId::new("acme");
        store.record_customer(&acme, "cus_ABC").unwrap();
        assert_eq!(store.get_customer(&acme).as_deref(), Some("cus_ABC"));
        store.record_customer(&acme, "cus_XYZ").unwrap();
        assert_eq!(store.get_customer(&acme).as_deref(), Some("cus_XYZ"));
        assert!(store.get_customer(&OrgId::new("globex")).is_none());
    }

    #[tokio::test]
    async fn signup_rejects_missing_bearer() {
        let cfg = BillingConfig {
            stripe_secret_key: "sk_test_x".into(),
            portal_return_url: "http://ok".into(),
            signup_token: "admin".into(),
            webhook_signing_secret: None,
            signup_verify_url: "http://localhost:8080/v1/signup/verify".into(),
            signup_token_ttl_secs: 900,
        };
        let tokens = ApiTokens::default();
        let store = InMemoryBillingStore::default();
        let stripe = StripeClient::new("sk_test_x");
        let req = SignupRequest {
            email: "root@example.com".into(),
            org: "Acme".into(),
        };
        let err = signup(&cfg, &tokens, &store, &stripe, None, req)
            .await
            .unwrap_err();
        assert!(matches!(err, BillingError::BadSignupToken));
    }

    #[tokio::test]
    async fn signup_rejects_wrong_bearer() {
        let cfg = BillingConfig {
            stripe_secret_key: "sk_test_x".into(),
            portal_return_url: "http://ok".into(),
            signup_token: "admin".into(),
            webhook_signing_secret: None,
            signup_verify_url: "http://localhost:8080/v1/signup/verify".into(),
            signup_token_ttl_secs: 900,
        };
        let tokens = ApiTokens::default();
        let store = InMemoryBillingStore::default();
        let stripe = StripeClient::new("sk_test_x");
        let req = SignupRequest {
            email: "root@example.com".into(),
            org: "Acme".into(),
        };
        let err = signup(&cfg, &tokens, &store, &stripe, Some("nope"), req)
            .await
            .unwrap_err();
        assert!(matches!(err, BillingError::BadSignupToken));
    }

    #[tokio::test]
    async fn signup_rejects_empty_org_slug() {
        let cfg = BillingConfig {
            stripe_secret_key: "sk_test_x".into(),
            portal_return_url: "http://ok".into(),
            signup_token: "admin".into(),
            webhook_signing_secret: None,
            signup_verify_url: "http://localhost:8080/v1/signup/verify".into(),
            signup_token_ttl_secs: 900,
        };
        let tokens = ApiTokens::default();
        let store = InMemoryBillingStore::default();
        let stripe = StripeClient::new("sk_test_x");
        let req = SignupRequest {
            email: "root@example.com".into(),
            org: "!!!".into(),
        };
        let err = signup(&cfg, &tokens, &store, &stripe, Some("admin"), req)
            .await
            .unwrap_err();
        assert!(matches!(err, BillingError::InvalidOrg));
    }

    #[tokio::test]
    async fn billing_portal_404s_for_unknown_org() {
        let cfg = BillingConfig {
            stripe_secret_key: "sk_test_x".into(),
            portal_return_url: "http://ok".into(),
            signup_token: "admin".into(),
            webhook_signing_secret: None,
            signup_verify_url: "http://localhost:8080/v1/signup/verify".into(),
            signup_token_ttl_secs: 900,
        };
        let store = InMemoryBillingStore::default();
        let stripe = StripeClient::new("sk_test_x");
        let err = billing_portal(&cfg, &store, &stripe, &OrgId::new("nobody"))
            .await
            .unwrap_err();
        match err {
            BillingError::NoCustomerForOrg(s) => assert_eq!(s, "nobody"),
            other => panic!("expected NoCustomerForOrg, got {other:?}"),
        }
    }

    // -------- Self-serve signup (magic-link) tests -----------------

    #[test]
    fn hash_signup_token_is_deterministic() {
        let a = hash_signup_token("hello");
        let b = hash_signup_token("hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64); // sha256 → 32 bytes → 64 hex chars
    }

    #[test]
    fn hash_signup_token_differs_between_inputs() {
        assert_ne!(hash_signup_token("a"), hash_signup_token("b"));
    }

    #[test]
    fn mint_signup_token_is_url_safe_32_chars() {
        let t = mint_signup_token();
        assert_eq!(t.len(), 32, "24 bytes → 32 base64url chars");
        for ch in t.chars() {
            assert!(
                ch.is_ascii_alphanumeric() || ch == '-' || ch == '_',
                "non-URL-safe char {ch:?} in {t:?}"
            );
        }
    }

    #[test]
    fn in_memory_pending_signup_round_trips() {
        let store = InMemoryBillingStore::default();
        let now = crate::time::rfc3339_now();
        let expires_at = crate::time::rfc3339_offset(60);
        let signup = PendingSignup {
            token_hash: hash_signup_token("secret-token"),
            email: "a@example.com".into(),
            org_name: "Acme".into(),
            created_at: now.clone(),
            expires_at,
        };
        store.record_pending_signup(&signup).unwrap();
        let taken = store
            .take_pending_signup(&signup.token_hash, &now)
            .expect("must return the row before expiry");
        assert_eq!(taken.email, "a@example.com");
        assert_eq!(taken.org_name, "Acme");
        // Consumed → second take is None.
        assert!(store
            .take_pending_signup(&signup.token_hash, &now)
            .is_none());
    }

    #[test]
    fn in_memory_pending_signup_expiry_is_enforced_at_take() {
        let store = InMemoryBillingStore::default();
        let signup = PendingSignup {
            token_hash: hash_signup_token("secret-token"),
            email: "a@example.com".into(),
            org_name: "Acme".into(),
            created_at: "2020-01-01T00:00:00.000Z".into(),
            expires_at: "2020-01-01T00:00:01.000Z".into(),
        };
        store.record_pending_signup(&signup).unwrap();
        // `now` is one second after expiry.
        let now = "2020-01-01T00:00:02.000Z";
        assert!(store.take_pending_signup(&signup.token_hash, now).is_none());
    }

    #[test]
    fn in_memory_re_request_replaces_prior_token_for_same_email() {
        let store = InMemoryBillingStore::default();
        let expires_at = crate::time::rfc3339_offset(60);
        let a = PendingSignup {
            token_hash: hash_signup_token("token-a"),
            email: "a@example.com".into(),
            org_name: "Acme".into(),
            created_at: crate::time::rfc3339_now(),
            expires_at: expires_at.clone(),
        };
        let b = PendingSignup {
            token_hash: hash_signup_token("token-b"),
            email: "a@example.com".into(),
            org_name: "Acme".into(),
            created_at: crate::time::rfc3339_now(),
            expires_at,
        };
        store.record_pending_signup(&a).unwrap();
        store.record_pending_signup(&b).unwrap();
        let now = crate::time::rfc3339_now();
        // Old token invalid.
        assert!(store.take_pending_signup(&a.token_hash, &now).is_none());
        // New token good.
        assert!(store.take_pending_signup(&b.token_hash, &now).is_some());
    }

    #[test]
    fn in_memory_gc_removes_expired_rows() {
        let store = InMemoryBillingStore::default();
        let expired = PendingSignup {
            token_hash: "expiredhash".into(),
            email: "expired@example.com".into(),
            org_name: "X".into(),
            created_at: "2020-01-01T00:00:00.000Z".into(),
            expires_at: "2020-01-01T00:00:01.000Z".into(),
        };
        let fresh = PendingSignup {
            token_hash: "freshhash".into(),
            email: "fresh@example.com".into(),
            org_name: "Y".into(),
            created_at: crate::time::rfc3339_now(),
            expires_at: crate::time::rfc3339_offset(60),
        };
        store.record_pending_signup(&expired).unwrap();
        store.record_pending_signup(&fresh).unwrap();
        let now = crate::time::rfc3339_now();
        assert_eq!(store.gc_expired_signups(&now).unwrap(), 1);
        // Fresh survived.
        assert!(store.take_pending_signup(&fresh.token_hash, &now).is_some());
    }

    #[tokio::test]
    async fn signup_request_returns_opaque_ok_for_malformed_email() {
        let store = InMemoryBillingStore::default();
        let email = LogEmailSender;
        let resp = signup_request(
            &store,
            &email,
            "https://ex.co/verify",
            60,
            SignupRequestRequest {
                email: "not-an-email".into(),
                org: "Acme".into(),
            },
        )
        .await
        .unwrap();
        assert!(resp.message.contains("If that address is eligible"));
        // No pending row was persisted for the malformed input.
        assert_eq!(
            store
                .gc_expired_signups(&crate::time::rfc3339_offset(3600))
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn signup_request_persists_pending_and_sends_email() {
        let store = InMemoryBillingStore::default();
        let email = LogEmailSender;
        signup_request(
            &store,
            &email,
            "https://ex.co/verify",
            60,
            SignupRequestRequest {
                email: "founder@example.com".into(),
                org: "Acme Inc".into(),
            },
        )
        .await
        .unwrap();
        // A pending signup landed — GC-with-past-expiry-cutoff removes 0
        // (because the row's expiry is in the future).
        let now_past = "2020-01-01T00:00:00.000Z";
        assert_eq!(store.gc_expired_signups(now_past).unwrap(), 0);
        // GC-with-future-cutoff removes it.
        assert_eq!(
            store
                .gc_expired_signups(&crate::time::rfc3339_offset(3600))
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn signup_verify_rejects_unknown_token() {
        let store = InMemoryBillingStore::default();
        let stripe = StripeClient::new("sk_test_x");
        let tokens = ApiTokens::from_csv("");
        let err = signup_verify(
            &tokens,
            &store,
            &stripe,
            SignupVerifyRequest {
                token: "never-issued".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BillingError::InvalidSignupToken));
    }
}
