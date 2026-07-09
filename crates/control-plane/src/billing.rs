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
}

impl std::fmt::Debug for BillingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BillingConfig")
            .field("stripe_secret_key", &"<redacted>")
            .field("portal_return_url", &self.portal_return_url)
            .field("signup_token", &"<redacted>")
            .finish()
    }
}

impl BillingConfig {
    /// `None` when any required env var is unset.
    pub fn from_env() -> Option<Self> {
        Some(Self {
            stripe_secret_key: std::env::var("STRIPE_SECRET_KEY").ok()?,
            portal_return_url: std::env::var("STRIPE_BILLING_PORTAL_RETURN_URL").ok()?,
            signup_token: std::env::var("NANOVM_SIGNUP_TOKEN").ok()?,
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

/// Persistence for the org → Stripe customer id mapping.
pub trait BillingStore: Send + Sync + std::fmt::Debug {
    fn record_customer(&self, org: &OrgId, customer_id: &str) -> Result<(), BillingStoreError>;
    fn get_customer(&self, org: &OrgId) -> Option<String>;
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
    map: Mutex<HashMap<OrgId, String>>,
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

// -------- Route wiring -------------------------------------------------

/// Sub-state carried on [`crate::AppState`] when the `billing` feature
/// is enabled at compile-time AND `BillingConfig::from_env()` returned
/// `Some` at startup. Access via [`crate::AppState::billing`].
#[derive(Clone)]
pub struct BillingCtx {
    pub config: BillingConfig,
    pub store: Arc<dyn BillingStore>,
    pub stripe: Arc<StripeClient>,
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

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    raw.strip_prefix("Bearer ").map(str::to_string)
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
