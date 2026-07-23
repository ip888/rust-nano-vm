# nanovm-web

Customer-facing dashboard for the `nanovm-control-plane` API. Landing page, self-serve signup, and a live "plan + usage" dashboard.

Fully client-side. Every page is either a static route or a client component; there's no server-side state and no server-side session — the API key you get after signup IS the session, kept in `localStorage`.

## Pages

| Route | Purpose |
|---|---|
| `/` | Landing page + pitch + code snippet. |
| `/pricing` | Public pricing tiers (Free / Pro / Team / Enterprise), competitor comparison, FAQ. Static route. |
| `/signup` | Email + org form → POST `/v1/signup/request`. |
| `/signup/verify` | Magic-link landing (`?token=…`). POSTs `/v1/signup/verify`, shows the API key once, persists the session. |
| `/login` | Paste API key. Verifies against `/v1/billing/plan` before caching. |
| `/dashboard` | Reads `/v1/billing/plan` + `/v1/usage`. Tiles for plan, usage, quick-start snippet. Sign-out clears the session. |
| `/dashboard/keys` | List, mint, revoke runtime API keys against `/v1/keys`. New keys shown once with copy-to-clipboard. |
| `/dashboard/vms` | List the org's VMs against `/v1/vms` (cursor-paginated). Destroy per row. |
| `/dashboard/snapshots` | List the org's snapshots against `/v1/snapshots` (cursor-paginated). Destroy per row. |

## Requirements

- Node **18.18** or newer (Next.js 15's minimum). Any current LTS (20 or 22) is fine.
- A running `nanovm-control-plane` reachable from the browser.
- The control plane MUST have `NANOVM_CORS_ORIGIN` set to the dashboard's origin, e.g.:

```bash
NANOVM_CORS_ORIGIN=http://localhost:3000 nanovm-control-plane
```

Otherwise the browser drops every cross-origin response — GETs like `/v1/billing/plan` too, not just mutating calls.

## Local dev

```bash
cd web
npm install
cp .env.example .env.local
# .env.local defaults are correct for `nanovm-control-plane` on
# 127.0.0.1:8080 — edit if you moved the API.

npm run dev
# open http://localhost:3000
```

Type-check + build:

```bash
npm run typecheck
npm run build
```

## Prod deploy

Two shapes, pick whichever fits:

**1. Vercel / Node runtime.**

```bash
npm run build && npm start
# NEXT_PUBLIC_NANOVM_API_URL=https://api.your-saas.com npm start
```

Set `NEXT_PUBLIC_NANOVM_API_URL` in the platform's env config.

**2. Container.**

Multi-stage `Dockerfile.web` at the repo root builds a distroless-node image (~200 MB).

```bash
# From the repo root, NOT from web/.
docker build -f Dockerfile.web \
  --build-arg NANOVM_API_URL=https://api.your-saas.com \
  -t nanovm-web:local .

docker run --rm -p 3000:3000 nanovm-web:local
```

The API host is baked at build time (Next.js inlines `NEXT_PUBLIC_*` into the client bundle). Rebuild the image when you change deploy targets, or pass a different `--build-arg NANOVM_API_URL=…` per environment.

## Live fork-latency demo on the landing page

The `<LiveForkBenchmark />` component on `/` runs 20 real fork
requests against a public demo tenant and renders a bar chart with
p50 / p95 / min / max. When the env vars below are unset (the
default), the component runs in **seeded mode** — the button shows a
plausible pre-baked dataset with a clear "Seeded — not live" pill so
visitors don't mistake it for real numbers.

To turn on live mode, set these at build time:

```bash
NEXT_PUBLIC_NANOVM_DEMO_URL=https://demo-api.your-saas.com   # public control plane
NEXT_PUBLIC_NANOVM_DEMO_TOKEN=demo-tenant-throwaway-token     # bearer for the demo tenant

# Pick ONE of these to name what to fork:
NEXT_PUBLIC_NANOVM_DEMO_SNAPSHOT_ID=42                        # a snapshot id, OR
NEXT_PUBLIC_NANOVM_DEMO_MARKETPLACE_NAME=python-3.12-minimal  # marketplace entry name (default)
```

**Security note:** the demo tenant's token IS baked into the client
bundle — it's public. Provision the demo tenant with a tight
`NANOVM_FORK_RPS` cap + a dedicated org id you can revoke, and never
reuse a paying-customer token there.

## Wiring against a real Stripe subscription

For the "Manage billing" button on the dashboard to work, the control plane needs:

- `STRIPE_SECRET_KEY` set (any `sk_test_…` works for dev).
- `STRIPE_BILLING_PORTAL_RETURN_URL` set to the dashboard URL — Stripe sends the user back here after they finish in the portal.

See `docs/saas-billing.md` for the full operator guide.

## What's NOT here yet

- **Live usage graph.** Today's tile shows totals; the "forks over the last 24 h" chart wants an SSE stream from the control plane, which is a separate PR.
- **Password / SSO / MFA.** Signup is magic-link only.
- **Framework component library.** Tailwind + hand-rolled components. If the design grows, migrating to shadcn/ui is straightforward.
