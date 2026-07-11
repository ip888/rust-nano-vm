# nanovm-web

Customer-facing dashboard for the `nanovm-control-plane` API. Landing page, self-serve signup, and a live "plan + usage" dashboard.

Fully client-side. Every page is either a static route or a client component; there's no server-side state and no server-side session — the API key you get after signup IS the session, kept in `localStorage`.

## Pages

| Route | Purpose |
|---|---|
| `/` | Landing page + pitch + code snippet. |
| `/signup` | Email + org form → POST `/v1/signup/request`. |
| `/signup/verify` | Magic-link landing (`?token=…`). POSTs `/v1/signup/verify`, shows the API key once, persists the session. |
| `/login` | Paste API key. Verifies against `/v1/billing/plan` before caching. |
| `/dashboard` | Reads `/v1/billing/plan` + `/v1/usage`. Tiles for plan, usage, quick-start snippet. Sign-out clears the session. |

## Requirements

- Node **20** or newer (Next.js 15 dropped 18).
- A running `nanovm-control-plane` reachable from the browser.
- The control plane MUST have `NANOVM_CORS_ORIGIN` set to the dashboard's origin, e.g.:

```bash
NANOVM_CORS_ORIGIN=http://localhost:3000 nanovm-control-plane
```

Otherwise the browser drops every `POST /v1/*` response.

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

The build output is standalone; a minimal Dockerfile would be `FROM node:22-alpine`, `COPY web/`, `npm run build`, `CMD ["npm", "start"]`. Kept as a follow-up so this MVP PR stays surgical.

## Wiring against a real Stripe subscription

For the "Manage billing" button on the dashboard to work, the control plane needs:

- `STRIPE_SECRET_KEY` set (any `sk_test_…` works for dev).
- `STRIPE_BILLING_PORTAL_RETURN_URL` set to the dashboard URL — Stripe sends the user back here after they finish in the portal.

See `docs/saas-billing.md` for the full operator guide.

## What's NOT here yet

- **API-key management page.** `/v1/keys` exists on the server; the UI to mint / list / revoke is a follow-up.
- **Live usage graph.** Today's tile shows totals; the "forks over the last 24 h" chart wants an SSE stream from the control plane, which is a separate PR.
- **Snapshot / VM management pages.** `/v1/snapshots`, `/v1/vms`. Same story: server surface exists, dashboard UI is a follow-up.
- **Password / SSO / MFA.** Signup is magic-link only.
- **Framework component library.** Tailwind + hand-rolled components. If the design grows, migrating to shadcn/ui is straightforward.
