# Launch readiness — from code-complete to paying customers

Where nanovm is today, and the concrete steps between here and the
first paying customer. Written for the operator running the SaaS
(you), not for end users.

## TL;DR — where the work sits

| Column | State | What's here |
|---|---|---|
| **Product code** | ≈ 100 % | Signup, billing (Stripe), dashboard (plan/usage/playground/keys/VMs/snapshots), Python SDK (`nanovm`), TypeScript SDK (`@nanovm/sdk`), marketplace snapshots, RBAC, audit sink, dunning, fork-latency benchmark tool, in-browser playground, onboarding checklist, free-tier upsell nudge → Stripe portal, /pricing + /why-nanovm + /marketplace, SEO (sitemap, robots, OG images, JSON-LD, RSS), 6 blog posts. |
| **Ops infra** | ≈ 20 % | Nothing is deployed yet. No domain, no Stripe account with configured products, no live control plane, no email delivery, no DNS. The current landing page URL `https://nanovm.example.com` is a placeholder in `NEXT_PUBLIC_NANOVM_WEB_ORIGIN`. |
| **Marketing** | ≈ 60 % | Landing + pricing + comparison + 6 blog posts + OG previews are shippable. Show-HN copy / Product Hunt submission / Twitter thread / demo GIF are not written. |

The remaining work is almost entirely **operator-side setup**, not
code. Every step below is either running a command, filling a form
on a third-party dashboard, or writing a post. None of it needs
another PR to the codebase in the base case.

---

## Order of operations (three-day plan)

### Day 1 — infrastructure stand-up

Everything here is a one-time setup on external providers. Total
elapsed time: a working day. Actual keyboard time: ≈ 2 hours.

1. **Buy the domain**. Namecheap / Cloudflare Registrar. `~$10/yr`.
2. **Cloudflare account** for DNS + SSL. Free tier. Add the domain
   and use Cloudflare's nameservers.
3. **Stripe account** in test mode.
   - Create three Products: `Free`, `Pro`, `Team`.
   - Create a monthly Price for each ($0 / $29 / $199).
   - Copy the `price_XXXX` ids — they land in `NANOVM_PLAN_TIERS`.
   - Configure the Customer Portal (Billing → Customer portal):
     enable "customer can update subscription", show all three
     products, set business info + return URL.
   - Copy the API key (`sk_test_...`) and webhook signing secret
     (`whsec_...`).
4. **Marketing site** — deploy to Vercel.
   - `vercel --prod` from `web/`.
   - Point the apex domain at the Vercel deployment.
   - Set env vars in Vercel dashboard:
     - `NEXT_PUBLIC_NANOVM_API_URL=https://api.<your-domain>`
     - `NEXT_PUBLIC_NANOVM_WEB_ORIGIN=https://<your-domain>`
5. **Control plane** — deploy to Fly.io (the `deploy/live-demo/`
   scripts already cover this).
   - `fly launch` inside `deploy/live-demo/`.
   - `fly volumes create` for snapshot storage.
   - `fly secrets set NANOVM_API_TOKENS=... STRIPE_SECRET_KEY=sk_test_... STRIPE_WEBHOOK_SIGNING_SECRET=whsec_... NANOVM_PLAN_TIERS=... NANOVM_CORS_ORIGIN=https://<your-domain> NANOVM_SMTP_URL=...`
   - Point `api.<your-domain>` at the Fly app.
6. **Email delivery** — Postmark or SES for the signup magic link.
   - Configure `NANOVM_SMTP_URL` on the control plane.
   - Verify the from-domain (DKIM + SPF via Cloudflare DNS).
7. **Stripe webhook** — from Stripe dashboard, add endpoint
   `https://api.<your-domain>/v1/stripe/webhook`. Copy the signing
   secret to `NANOVM_STRIPE_WEBHOOK_SIGNING_SECRET` on the control
   plane.
8. **First smoke test** — from your own laptop, run the signup
   flow. Complete a magic link. Copy the returned API key. Hit
   `/v1/health`. Fork a marketplace snapshot. All happy path.

### Day 2 — SDK distribution + trust plumbing

1. **PyPI Trusted Publisher for `nanovm`** — one-time.
   - Register the pending publisher on
     https://pypi.org/manage/account/publishing/ pointing at
     `.github/workflows/python-publish.yml`, environment `pypi`.
   - Push a `v0.1.0` git tag. The workflow publishes the package.
   - Verify `pip install nanovm` from a clean venv.
2. **npm scope + Trusted Publisher for `@nanovm/sdk`** — one-time.
   - Create the `@nanovm` scope on npmjs.com.
   - First publish is manual from your machine: `cd clients/typescript && npm publish --access public --provenance` after `npm login`.
   - Once the package exists, configure Trusted Publisher on npmjs
     pointing at `.github/workflows/npm-publish.yml`, environment
     `npm`.
   - From then on, every `v*.*.*` tag ships both SDKs together.
3. **Docker image** — `ghcr.io/<you>/nanovm-control-plane:latest`
   is already pushed by `.github/workflows/docker.yml`. Verify it
   pulls + boots from an empty machine.

### Day 3 — public demo tenant + launch prep

1. **Public demo tenant** on the control plane.
   - Create a throwaway org `demo` with a heavily rate-limited
     token (`NANOVM_FORK_RPS_demo=1`).
   - The token IS public — it's baked into the landing page's
     `NEXT_PUBLIC_NANOVM_DEMO_TOKEN`. Re-set the marketing
     site env vars and redeploy the web app.
   - Confirm the landing page's LiveForkBenchmark pill flips from
     "Seeded — not live" to "Live" and shows real numbers.
2. **Launch content** — ready-to-paste drafts live in
   [`docs/launch/`](launch/README.md):
   - [`show-hn.md`](launch/show-hn.md) — post + top-comment
     technical deep-dive + prepared defenses for the four most
     likely follow-up questions.
   - [`twitter-thread.md`](launch/twitter-thread.md) — 6-tweet
     thread with the LiveForkBenchmark GIF as the hook.
   - [`linkedin.md`](launch/linkedin.md) — the same story
     reshaped for LinkedIn's audience.
   - [`producthunt.md`](launch/producthunt.md) — Product Hunt
     submission with gallery, description, first comment.
   - [`dev-to-crosspost-05.md`](launch/dev-to-crosspost-05.md)
     + [`06`](launch/dev-to-crosspost-06.md) — dev.to versions
     of blog posts 05 (Claude Code) and 06 (LangChain.js),
     each with the "hosted on nanovm — free tier" footer.
   - [`metrics.md`](launch/metrics.md) — the two Prometheus
     alerts that predict trouble + comment-triage table + when
     to declare victory or defeat 24 h in.
3. **Support surface** — decide before launch:
   - GitHub issues (already there).
   - Optional Discord + a `SECURITY.md` for responsible
     disclosure. See the enterprise-adjacent parked list below if
     you want to ship these before launch.

---

## Launch-day checklist (T-minus 0)

- [ ] Landing page loads on the real domain.
- [ ] `/pricing` shows real numbers, "Start free" goes to signup.
- [ ] Signup → magic-link email arrives inside 30 seconds.
- [ ] Verification → dashboard renders → onboarding checklist
      appears → step 1 auto-detects on first API call.
- [ ] `/dashboard/playground` runs `print(1+1)` against a real
      KVM microVM in < 1 s (second run).
- [ ] `pip install nanovm && python -c 'import nanovm; ...'`
      round-trips against the deployed API.
- [ ] `npm install @nanovm/sdk` succeeds.
- [ ] The Stripe portal opens from the dashboard, shows the tier
      picker for a Free-tier user, redirects back to the dashboard.
- [ ] A test $29 charge lands, subscription status flips to
      `active`, plan tile reflects `Pro`.
- [ ] The free-tier nudge disappears for Pro users.
- [ ] `/rss.xml`, `/sitemap.xml`, `/robots.txt` all serve
      correctly and reference the real domain.
- [ ] LiveForkBenchmark on the landing page runs actual forks
      against the demo tenant.
- [ ] OG image previews render on
      https://cards-dev.twitter.com/validator and
      https://opengraph.xyz.
- [ ] `curl -sI https://api.<domain>/v1/health` returns 200.

Only after every box is ticked does the Show HN / Product Hunt
launch fire.

---

## Post-launch — first 24 hours

- Monitor `/metrics` (Prometheus scrape) for fork RPS spikes.
- Watch Stripe dashboard for first paying conversions.
- Reply to every HN comment inside 15 min for the first two
  hours (single biggest driver of top-page dwell time).
- Screenshot a real "paying customer" the moment it arrives — it
  becomes tomorrow's tweet.

---

## Parked (enterprise, not mass-user launch blockers)

- **SSO / SAML / SCIM** — Clerk or WorkOS, integrated when a real
  enterprise prospect materializes. `docs/rbac.md` covers the
  shape.
- **`SECURITY.md` + threat model + compliance mapping** — helpful
  the moment an enterprise procurement asks. Ship in the two
  weeks between launch and first enterprise inbound.
- **Cosign container signing** — SLSA-3 for the Docker image.
  Small PR, enterprise-adjacent, not gating self-serve
  conversions.
- **Structured event stream (Kafka / NATS)** — for the audit
  sink's higher-volume consumers. Follow-up to the HTTP sink.

---

## The honest answer to "will people use this?"

The three questions a mass-user prospect asks in the first 30
seconds:

1. **"Does this actually work?"** — LiveForkBenchmark on the
   landing page, in-browser playground behind signup, real KVM
   integration test in the bench crate. Yes, honestly.
2. **"How much?"** — /pricing page with a real Free tier that
   doesn't require a card. Yes, transparent.
3. **"Why not the incumbent (E2B / Modal)?"** — /why-nanovm has
   a straight comparison table that recommends the incumbent when
   they're the right pick. Yes, honest.

The mass-user gap is not the product — it's discoverability +
distribution. Every remaining item in the "Day 3" list above is
about making sure the right people find out this exists.
