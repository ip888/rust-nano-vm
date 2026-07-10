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
/// whitespace + trailing commas. A default fallback (when the caller
/// has no subscription) uses `default_rps`, which comes from the
/// existing `NANOVM_FORK_RPS` env var (already read by [`crate::ForkQuota`]).
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
            by_price_id.insert(
                price_id.trim().to_string(),
                PlanTier {
                    name: name.trim().to_string(),
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
    /// 503 `billing_disabled`. Never appears in Debug output.
    pub webhook_signing_secret: Option<String>,
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
    /// RFC 3339 timestamp of the last update — for observability +
    /// operator triage. Set by the store on write.
    pub updated_at: String,
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
}

mod sqlite_billing_backend;
pub use sqlite_billing_backend::SqliteBillingStore;

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
    pub async fn create_customer(
        &self,
        email: &str,
        name: &str,
        org_slug: &str,
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

// -------- Handler functions -------------------------------------------

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
    if bearer != Some(cfg.signup_token.as_str()) {
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
    //    mutation, so a retry is safe.
    let customer = stripe
        .create_customer(&req.email, &req.org, &org_slug)
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
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
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
    if (now - ts).abs() > tolerance_secs {
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
    // Primary subscription item's price id. Stripe multi-item
    // subscriptions do exist; we pick the first one which matches the
    // common single-item shape most SaaS deploys use. A follow-up can
    // extend this to enumerate multi-item subscriptions.
    let price_id = object
        .get("items")
        .and_then(|v| v.get("data"))
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("price"))
        .and_then(|price| price.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok((
        customer_id,
        SubscriptionState {
            subscription_id,
            status,
            price_id,
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
            ApiError::Bad(format!("stripe api error ({status}): {message}"))
        }
        BillingError::StripeTransport(msg) => {
            ApiError::InternalDyn(format!("stripe transport error: {msg}"))
        }
        BillingError::Store(inner) => ApiError::InternalDyn(inner.to_string()),
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
}
