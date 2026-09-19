# Autonomous production ops — the one-page runbook

> **Goal**: ~40 minutes of human work → a live SaaS that keeps
> running, self-heals, and ships incremental improvements without
> you touching it. (The steps below individually sum to 40 min —
> 5 domain + 10 accounts + 10 credentials + 5 bootstrap + 10 CI wiring.
> Faster with prior experience of Cloudflare/Vercel/Fly/Stripe.)
>
> **What "autonomous" means honestly**: everything that doesn't
> require your legal identity (account creation, payment methods,
> KYC) is done for you. The three things that legally can't be
> automated are called out below with checklists.

## The architecture

```
                 ┌──────────────────┐
                 │ You              │
                 │ (30-min setup,   │
                 │  then hands-off) │
                 └────────┬─────────┘
                          │  gathers credentials once
                          ▼
                 ┌──────────────────────┐
                 │ scripts/launch/*.sh  │  ← one-shot bootstrap
                 └────────┬─────────────┘
                          │  runs the deploy chain
       ┌──────────────────┼──────────────────────┐
       ▼                  ▼                      ▼
 ┌───────────┐      ┌──────────┐            ┌──────────┐
 │ Cloudflare│      │  Vercel  │            │  Fly.io  │
 │   (DNS)   │      │(marketing)│           │(control  │
 └───────────┘      └──────────┘            │  plane)  │
                                            └──────────┘
                          ▲                      ▲
                          │  every push to main  │
                          │  auto-deploys        │
                    ┌─────┴──────────────────────┴─────┐
                    │        GitHub Actions            │
                    │  .github/workflows/deploy.yml    │
                    │  .github/workflows/*-publish.yml │
                    └──────────────────────────────────┘
                                     ▲
                                     │ open PRs, merge on green
                    ┌────────────────┴──────────────────┐
                    │       Dependabot (weekly)         │
                    │  cargo + npm + pip + gh-actions   │
                    └───────────────────────────────────┘
```

## What runs itself once bootstrap is done

| Loop | Runs | Trigger | What it does |
|---|---|---|---|
| **CI/CD deploy** | On every merge to `main` | GitHub push event | Test → build → deploy web + control-plane → smoke. A failure in the deploy step leaves the previous release live (Fly + Vercel each retain the previous version on failure). A failure in the *smoke* step does NOT auto-roll-back — the just-deployed version stays live and the run is marked red; investigate before merging further changes. |
| **SDK ship** | On every `v*.*.*` git tag | Push tag | Publishes `nanovm` to PyPI + `@nanovm/sdk` to npm via Trusted Publishers (zero long-lived secrets). |
| **Dependency updates** | Weekly, Monday morning UTC | Dependabot | Opens PRs to bump cargo / npm / pip / gh-actions deps. |
| **Auto-merge** | Every Dependabot PR | GitHub PR event | Merges the PR the moment all required checks pass. |
| **Uptime + billing alerts** | Continuous (with the extra setup step below) | Prometheus / Fly checks | Fly notifies you if the machine crash-loops; Stripe emails you on failed payments; both are self-fixing (Fly restarts, Stripe retries dunning per `NANOVM_DUNNING_GRACE_HOURS`). **Prometheus alerts are NOT wired by Steps 1–5** — see "Optional: hook up Prometheus alerts" below. |
| **Copilot review sweeps** | On every PR you open | GitHub Copilot review bot | Not you-triggered; Copilot posts findings and you address them via the PR sweep pattern already established in the repo (see #200, #196, #181). |

Everything above is **already shipped** in this repo. The remaining
piece — `.github/workflows/deploy.yml` — lands in this same PR.

## The 30-minute bootstrap (the ONE thing you have to do yourself)

### Step 1 — buy a domain (5 min)

Any registrar works. Cloudflare Registrar is cheapest at cost + gives
you Cloudflare DNS for free. Popular choices:

- `nanovm.dev`
- `nanovm.io`
- `nanovm.ai`
- `<yourbrand>.com` if this is a rebrand

### Step 2 — create the seven service accounts (10 min)

Sign up (free tiers where noted) for all seven in a browser. No CLI yet.

| Account | Why | Cost |
|---|---|---|
| [Cloudflare](https://dash.cloudflare.com/sign-up) | DNS + free SSL | Free |
| [Vercel](https://vercel.com/signup) | Marketing site hosting | Free tier |
| [Fly.io](https://fly.io/app/sign-up) | Control-plane hosting (needs KVM) | Add credit card; ~$25/mo baseline |
| [Stripe](https://dashboard.stripe.com/register) | Billing | Free until first charge |
| [Resend](https://resend.com/signup) | Magic-link email (the control-plane binary only wires the Resend provider — `RESEND_API_KEY` + `NANOVM_SIGNUP_FROM`) | Free up to 100 emails/day; ~$20/mo after |
| [npmjs](https://www.npmjs.com/signup) | `@nanovm/sdk` publish | Free |
| [PyPI](https://pypi.org/account/register/) | `nanovm` publish | Free |

Add a credit card to Fly.io only — the others are free tier for
launch.

### Prerequisite tools

The launch scripts drive Fly, Vercel, Stripe, Cloudflare, and GitHub
through their CLIs. Install these once on the machine you'll run
`scripts/launch/preflight.sh` from:

```sh
# macOS
brew install flyctl vercel-cli gh stripe/stripe-cli/stripe jq openssl bind

# Ubuntu / Debian
curl -L https://fly.io/install.sh | sh                             # flyctl
sudo apt-get install -y curl jq openssl dnsutils                   # curl, jq, openssl, dig
type -p curl >/dev/null || sudo apt install curl -y                # gh prereq
curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
    | sudo dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg \
 && sudo chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg \
 && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
    | sudo tee /etc/apt/sources.list.d/github-cli.list \
 && sudo apt update && sudo apt install gh -y                      # gh
curl -fsSL https://cli.vercel.com/install.sh | sh                  # vercel-cli
curl -s https://packages.stripe.dev/api/security/keypair/stripe-cli-gpg/public \
    | sudo gpg --dearmor -o /usr/share/keyrings/stripe.gpg \
 && echo "deb [signed-by=/usr/share/keyrings/stripe.gpg] https://packages.stripe.dev/stripe-cli-debian-local stable main" \
    | sudo tee /etc/apt/sources.list.d/stripe.list \
 && sudo apt update && sudo apt install stripe -y                  # stripe
```

`preflight.sh` reports each missing tool with a `[×]` line so this
list stays honest across OS variations.

### Step 3 — gather one bag of credentials (10 min)

Open the following URLs in tabs and copy each secret into a temporary
text file:

| Get | URL | Token shape |
|---|---|---|
| Cloudflare API token | https://dash.cloudflare.com/profile/api-tokens (Create Token → "Edit zone DNS" template → scope to your zone) | 40+ chars |
| Cloudflare Zone ID | https://dash.cloudflare.com/ → click domain → Overview → right sidebar | 32 hex chars |
| Vercel token | https://vercel.com/account/tokens (Create) | `vercel_...` |
| Fly.io token | terminal: `flyctl auth login` then `flyctl auth token` | multi-line JWT |
| Stripe secret key | https://dashboard.stripe.com/test/apikeys (start in test mode) | `sk_test_...` |
| Resend API key | https://resend.com/api-keys → "Create API Key" | `re_...` |

### Step 4 — bootstrap the launch (5 min)

```sh
git clone https://github.com/ip888/rust-nano-vm.git
cd rust-nano-vm

# Fill in the .env with the tokens from step 3.
cp scripts/launch/.env.example scripts/launch/.env
$EDITOR scripts/launch/.env

# One-shot bootstrap. Runs the full deploy chain.
./scripts/launch/preflight.sh          # verify tools + env
./scripts/launch/stripe-setup.sh       # create Free/Pro/Team on Stripe
./scripts/launch/cloudflare-dns.sh     # point DNS
./scripts/launch/vercel-deploy.sh      # deploy marketing site
./scripts/launch/fly-deploy.sh         # deploy control plane
./scripts/launch/stripe-webhook.sh     # register webhook (get whsec_...)

# Paste the whsec_... into .env, then re-run fly-deploy.
./scripts/launch/fly-deploy.sh

# Verify.
./scripts/launch/smoke.sh
```

If any step fails, fix the reported issue and re-run — every script
is idempotent.

### Step 5 — wire the CI/CD once (10 min, one-time)

The `.github/workflows/deploy.yml` shipped in this repo needs four
secrets AND three variables in the repo's Actions settings. Secrets
are opaque to the workflow (Fly + Vercel credentials); variables are
readable in logs (app + domain names — non-sensitive).

```sh
# ---- Secrets (four) ---------------------------------------------------
gh secret set FLY_API_TOKEN     --body "$(flyctl auth token)"
gh secret set VERCEL_TOKEN      --body "<vercel token from step 3>"
gh secret set VERCEL_ORG_ID     --body "<vercel dashboard → settings>"
gh secret set VERCEL_PROJECT_ID --body "<from web/.vercel/project.json after first deploy>"

# ---- Variables (three, non-secret) -----------------------------------
gh variable set NANOVM_FLY_APP    --body "nanovm-control-plane-prod"
gh variable set NANOVM_DOMAIN     --body "<your.domain>"
gh variable set NANOVM_API_DOMAIN --body "api.<your.domain>"
```

Two more one-time steps for SDK auto-publish:

- **PyPI Trusted Publisher** — https://pypi.org/manage/account/publishing/  
  Publisher: GitHub · Owner: `ip888` · Repo: `Rust-nano-vm` · Workflow: `python-publish.yml` · Environment: `pypi`.
- **npm Trusted Publisher** — https://www.npmjs.com/settings/nanovm/packages after your first manual `npm publish`. Point at `.github/workflows/npm-publish.yml`, environment `npm`.

That's it. Total human time: **~30 min once**, then **0 hrs/wk
ongoing** until you want to launch content (in `docs/launch/`) or
you get paged.

## Self-correction — what happens when things break

| Failure | Detection | Auto-recovery | You do |
|---|---|---|---|
| Fly machine crash | Fly checks | Restart within 10 s | Nothing |
| Control-plane deploy fails | GitHub Actions `deploy.yml` | Doesn't promote — previous version stays live | Read the CI log, push a fix, next merge re-tries |
| Vercel deploy fails | Vercel | Rolls back to previous | Same as above |
| Stripe webhook signature mismatch | 5xx from `/v1/stripe/webhook` | None | Rotate `STRIPE_WEBHOOK_SIGNING_SECRET`, re-run fly-deploy.sh |
| Card payment fails | Stripe | Dunning: past_due → unpaid → canceled per `NANOVM_DUNNING_GRACE_HOURS` (default 72h). Control plane returns 402 on fork. | Nothing — customer retries their card via the Stripe portal |
| CI on a Dependabot PR fails | GitHub Actions | PR stays open, no merge | Sweep the failure next time you check |
| Copilot posts findings on a PR | Copilot review bot | The PR sweep pattern in the repo is manual today | Address on your next active session |
| Rate-limit spike on the demo tenant | Prometheus counter | 429 responses; tenant token is capped by `NANOVM_FORK_RPS_demo` | Nothing unless traffic is coming from a real launch (then celebrate) |

## What's genuinely still hands-on (weekly cadence, ~15 min)

- **Launch content firing**. `docs/launch/show-hn.md` and friends are
  written; posting them is a human moment (Show HN + Product Hunt
  submissions each need you present to reply to comments in the
  first 2 hours).
- **Copilot findings sweep**. When you open a PR, Copilot may post
  review comments; the repo's PR sweep pattern (see #200, #196,
  #181) is your response.
- **Stripe → live-mode flip**. Test-mode charges don't count; when
  you're ready, complete Stripe KYC in the dashboard and swap
  `sk_test_...` → `sk_live_...` in `.env` + re-run `fly-deploy.sh`.

Everything else — dependency updates, code deploys, database
backups (Fly volumes), SSL renewal (Fly + Cloudflare), OS patches
(Fly's job) — is fully automated.

## Ongoing cost budget

Two shapes, depending on how you host the control plane:

### Always-on machine (what the shipped `deploy/live-demo/fly/fly.toml` picks)

| Line | Cost |
|---|---|
| Domain | ~$12/yr |
| Cloudflare | $0 (free tier) |
| Vercel | $0 (free tier, upgrade to $20/mo Pro when > 100 GB bandwidth/mo) |
| Fly.io control plane (`performance-2x`, 2 vCPU / 4 GB, `auto_stop_machines=off`, `min_machines_running=1`) — **~$0.30/hr × 730 h/mo ≈ $219/mo** | ~$219/mo |
| Fly.io storage volume (10 GB) | ~$1.50/mo |
| Resend email (Free tier covers 100 emails/day; upgrade past that) | $0 → ~$20/mo |
| Stripe fees | 2.9% + 30¢ per charge — no fixed cost |
| **Baseline** | **~$232/mo** before customers |

That's the honest number if you keep the machine hot 24×7 the way
the shipped Fly manifest does. Break-even: **9 Pro customers at
$29/mo**.

### Scale-to-zero (for pre-launch and light-traffic operation)

Flip `deploy/live-demo/fly/fly.toml` to `auto_stop_machines = "stop"`
and `min_machines_running = 0`, and set an `auto_start = true`
service. The machine sleeps between requests and pays only for the
seconds it's actually serving.

| Line | Cost |
|---|---|
| Domain | ~$12/yr |
| Cloudflare | $0 |
| Vercel | $0 |
| Fly.io control plane (idle most of the day, cold-start ~1 s on next request) | **~$5-15/mo** at pre-launch traffic |
| Fly.io storage volume (10 GB) | ~$1.50/mo |
| Resend email | Free tier |
| **Baseline** | **~$20/mo** pre-launch |

Cold-start cost is a ~1 s delay on the first request after quiet.
Fine for pre-launch and demo traffic; flip back to always-on when
paid usage starts driving the machine hot anyway.

## What to do RIGHT NOW to launch

1. Go through Steps 1–5 above (30 min).
2. Read [`docs/launch/README.md`](launch/README.md) to see the
   Show-HN post I've already drafted for you.
3. Pick a Tuesday-Thursday between 6:00-8:00 AM Pacific.
4. Post from `docs/launch/show-hn.md`, fire the Twitter thread 15
   min later, LinkedIn 3 hours after.
5. Watch [`docs/launch/metrics.md`](launch/metrics.md) for the
   two Prometheus alerts that predict trouble.

## The failure mode I want you to prepare for

**HN top-3 for 4+ hours** = ~20-50 signups per hour = the demo
tenant will hit `NANOVM_FORK_RPS_demo=1` almost immediately. This
is a good problem. Two responses:

- **If most traffic is signups**: nothing to do; the free tier
  spreads the load organically.
- **If the LiveForkBenchmark on the landing page bogs down**: raise
  the global fork-rate cap (the control plane parses global
  `NANOVM_FORK_RPS` / `NANOVM_FORK_BURST`; per-org caps come from
  Stripe plan tiers, so there is no `NANOVM_FORK_RPS_demo`
  per-tenant override):
  ```sh
  flyctl secrets set -a nanovm-control-plane-prod NANOVM_FORK_RPS=100 NANOVM_FORK_BURST=200
  ```
  Fly restarts the machine in ~30 seconds; nothing else changes.
  Tighten the Free-tier plan's fork quota via `NANOVM_PLAN_TIERS` if
  you want to prevent free traffic from starving paid traffic.

## Where to look when something feels wrong

```sh
./scripts/launch/status.sh            # 5-second overall health check
flyctl logs -a nanovm-control-plane-prod    # live control-plane logs
flyctl dashboard -a nanovm-control-plane-prod    # opens the Fly console
stripe events resend --stripe-account acct_... evt_...    # replay a webhook
gh pr list --state open --limit 20    # see all open PRs
gh run list --workflow deploy.yml --limit 10    # last 10 deploys
```

If you truly can't diagnose it, spin up a fresh Claude Code session
against this repo — the entire launch context is in
[`docs/launch-readiness.md`](launch-readiness.md), the visual
walkthrough artifact, and this file. Claude can pick it up cold.
