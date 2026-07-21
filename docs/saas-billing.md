# Running `nanovm` as a SaaS (Stripe billing)

The control plane ships with a self-serve **signup + billing portal + webhook + plan-enforcement** stack behind the `billing` cargo feature. This page is the operator's guide to turning it on.

The flow it implements:

1. Visitor lands on your marketing site, submits `POST /v1/signup` with an email + org name.
2. Server creates a **Stripe customer**, mints the first API key, persists the mapping in SQLite, returns `{api_key, org, stripe_customer_id}`.
3. Customer opens `GET /v1/billing/portal` → Stripe-hosted portal → picks a plan → returns to your app.
4. Stripe fires `customer.subscription.created`/`.updated`/`.deleted` + `invoice.paid`/`invoice.payment_failed` webhooks at your `POST /v1/stripe/webhook`.
5. The webhook handler verifies HMAC-SHA256, parses the subscription's `price_id`, persists it, and **the customer's per-org fork rate is now sized off their tier on the very next request** — no restart needed.
6. `GET /v1/billing/plan` (bearer-auth'd) returns `{plan, subscription_status, price_id}` so your dashboard can render "You're on Pro".

## Build

```bash
cargo build --release --features billing --bin nanovm-control-plane
# `billing` implies `sqlite`; the binary needs neither system libsqlite3 nor OpenSSL.
```

## Env vars

**All Stripe secrets stay in the environment.** Wire them via `flyctl secrets set` / K8s Secret / Helm values. Never commit these to git.

| Env var | Required? | What it is |
|---|---|---|
| `STRIPE_SECRET_KEY` | yes | `sk_test_…` in dev, `sk_live_…` in prod. |
| `STRIPE_BILLING_PORTAL_RETURN_URL` | yes | Where Stripe sends the customer after they finish in the portal. Typically your dashboard URL. |
| `NANOVM_SIGNUP_TOKEN` | yes | Admin bearer that gates `POST /v1/signup`. Rotate on demand. |
| `STRIPE_WEBHOOK_SIGNING_SECRET` | recommended | The `whsec_…` value from your Stripe dashboard's webhook endpoint. Without it, `POST /v1/stripe/webhook` returns 503 `billing_disabled`. |
| `NANOVM_OWNERSHIP_STORE` | yes for prod | Path to the SQLite file. Same file is used for billing state (they migrate independently). Without it, ownership + billing are in-memory only and lost on restart. |
| `NANOVM_PLAN_TIERS` | recommended | See below. Without it, all customers get the env-default fork rate. |

### `NANOVM_PLAN_TIERS`

Maps Stripe `price_id`s to `{tier_name, forks_per_second}`:

```
NANOVM_PLAN_TIERS=price_ABC=free:5,price_XYZ=pro:100,price_ENT=enterprise:1000
```

- `price_ABC` etc. are the Stripe **price** ids (not product ids), copied from your Stripe dashboard.
- `free`, `pro`, `enterprise` are the names your dashboard renders.
- Numbers are sustained forks-per-second (burst = same number, min 1).
- Malformed entries are logged at `warn` and skipped, so a typo can't take the whole billing subsystem offline.
- Customers with no mapped tier (or when the env var is unset) use `NANOVM_FORK_RPS` / `NANOVM_FORK_BURST`.

### Rate-limit knobs

- `NANOVM_FORK_RPS` (default `10`) — the env-default sustained rate.
- `NANOVM_FORK_BURST` (default = `RPS`) — bucket capacity.
- Setting `NANOVM_FORK_RPS=0` disables both the per-token and per-org buckets (the binary logs a `WARN`).

## Endpoints

### `POST /v1/signup`

```bash
curl -X POST https://api.your-saas.com/v1/signup \
  -H "Authorization: Bearer $NANOVM_SIGNUP_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"email": "founder@acme.example", "org": "Acme Inc"}'
```

Returns:

```json
{
  "org": "acme-inc",
  "api_key": "acme-inc:<64-char-secret>",
  "stripe_customer_id": "cus_ABC…"
}
```

The `api_key` is shown **once** — the server only stores its hash. Give it to the customer immediately.

Ordering guarantee: Stripe is called **before** the API key is minted, so a Stripe failure doesn't leave a phantom local key.

### `GET /v1/billing/portal`

Bearer-auth'd. Returns a short-lived Stripe portal URL for the caller's org:

```json
{ "url": "https://billing.stripe.com/p/session/…" }
```

Redirect the customer here from your dashboard.

### `GET /v1/billing/plan`

Bearer-auth'd. Returns the caller's current plan:

```json
{
  "plan": { "name": "pro", "rps": 100 },
  "subscription_status": "active",
  "price_id": "price_XYZ"
}
```

- `plan` is `null` when the caller has no active subscription mapped to a configured tier.
- `subscription_status` mirrors Stripe verbatim (`active`, `trialing`, `past_due`, `canceled`, `incomplete`, `paused`, `unpaid`).
- `price_id` is the raw Stripe id currently on file, useful for triaging why a subscription didn't map to a named tier (typo in `NANOVM_PLAN_TIERS`? new price id you forgot to configure?).

### `POST /v1/stripe/webhook`

**No auth header.** Signature verification uses `STRIPE_WEBHOOK_SIGNING_SECRET` against the `Stripe-Signature` header (`t=…,v1=…`; `v2` is supported by ignoring; multiple `v1` values are all tried, so key rotation works).

Handled event types:

| Event | Side effect |
|---|---|
| `customer.subscription.created` | Persist `{subscription_id, status, price_id}` for the customer. |
| `customer.subscription.updated` | Same. Tier changes flow into `ForkQuota` on the customer's next fork. |
| `customer.subscription.deleted` | Persist status `canceled`. Plan resolver returns `subscription_status: canceled`. |
| `invoice.paid` | Structured `info` log. Ops sees payments succeed. |
| `invoice.payment_failed` | Structured `warn` log with `hosted_invoice_url` (hand this to the customer to fix a declined card). |

All other event types return 200 (Stripe treats 2xx as delivered) and are counted in `nanovm_stripe_webhook_events_total{event_type="…"}` for observability.

### Enforcement dimensions

Every `POST /v1/snapshots/:id/fork` checks **two** independent buckets:

1. **Per-token** — sized by `NANOVM_FORK_RPS` / `NANOVM_FORK_BURST`. Protects against a runaway API key.
2. **Per-org** — sized by the caller's Stripe plan (`NANOVM_PLAN_TIERS` → `PlanTier.rps`). Protects your tier scheme against a customer minting extra API tokens.

Either failing returns `429 fork_quota_exceeded` with `Retry-After`.

## Stripe dashboard setup

1. **Create prices** for each tier in your Stripe dashboard, e.g. `price_free`, `price_pro`, `price_enterprise`. Copy the price ids.
2. **Point your webhook** at `https://api.your-saas.com/v1/stripe/webhook`. Select events: `customer.subscription.created`, `customer.subscription.updated`, `customer.subscription.deleted`, `invoice.paid`, `invoice.payment_failed`.
3. **Copy the `whsec_…`** signing secret into `STRIPE_WEBHOOK_SIGNING_SECRET`.
4. **Set `NANOVM_PLAN_TIERS`** with the price ids from step 1.
5. Restart the control-plane.

## Metrics

Ops dashboards to build:

- `rate(nanovm_forks_total_by_org{org="…"}[5m])` — per-org fork rate. The denominator any usage-based invoicing uses.
- `rate(nanovm_fork_quota_throttled_total_by_org{org="…"}[5m])` — 429 rate. A customer hitting their plan ceiling.
- `rate(nanovm_stripe_webhook_events_total{event_type="invoice.payment_failed"}[5m])` — invoice-failure rate. Wake someone up when this crosses zero.
- `rate(nanovm_stripe_webhook_events_total{event_type=~"customer.subscription.*"}[1h])` — subscription-lifecycle churn.

## Testing locally

Use Stripe's CLI:

```bash
stripe listen --forward-to localhost:8080/v1/stripe/webhook
# copy the whsec_… it prints into STRIPE_WEBHOOK_SIGNING_SECRET

stripe trigger customer.subscription.updated
stripe trigger invoice.payment_failed
```

Verify state landed:

```bash
sqlite3 nanovm.sqlite 'SELECT org, subscription_status, price_id FROM stripe_customers;'
```

## Known gaps + follow-ups

- **Metered billing**: `nanovm_forks_total_by_org` isn't yet reported to Stripe `usage_records`. Customers on metered prices won't be billed for overage until a follow-up ships the reporter task.
- **`subscription_item_id`**: not currently persisted; needed by the metered reporter above.
- **Multi-item subscriptions**: only the primary item's `price_id` is stored. Multi-item subs are rare in single-product SaaS; handle in a follow-up if needed.
- **Dunning enforcement** *(shipped)*: fork routes (`POST /v1/snapshots/:id/fork` and `POST /v1/marketplace/snapshots/:name/fork`) return **402 Payment Required** when `subscription_status` ∈ `{past_due, unpaid, canceled}` AND `now - subscription.updated_at > NANOVM_DUNNING_GRACE_HOURS`. Default grace is **72 h** — long enough that a weekend payment-method hiccup doesn't lock the customer out, short enough that we don't ship compute on unpaid subscriptions indefinitely. The 402 body extends the standard envelope with `upgrade_endpoint: "/v1/billing/portal"` so clients can render a "Manage billing" link. Blocked forks increment `nanovm_dunning_blocked_total{org, status}` on `/metrics`. Free-tier callers (no Stripe customer row) are never gated. Set `NANOVM_DUNNING_GRACE_HOURS=0` to enforce immediately, or an explicit higher value for longer grace.
