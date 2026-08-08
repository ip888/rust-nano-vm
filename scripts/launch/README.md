# scripts/launch — operator runbook for the mass-user launch

Executable scripts that stand up a production nanovm SaaS from a
clean laptop. Runs the [`docs/launch-readiness.md`](../../docs/launch-readiness.md)
3-day plan as actual commands, not prose.

## Prereqs — what to install once

```sh
# macOS (brew) — one-liner for the whole toolkit
brew install flyctl vercel gh stripe/stripe-cli/stripe jq curl

# Linux (Debian/Ubuntu) — separate installers
curl -L https://fly.io/install.sh | sh                          # flyctl
npm install -g vercel                                           # vercel
curl -sS https://webi.sh/gh | sh                                # gh
curl -sS https://raw.githubusercontent.com/stripe/stripe-cli/master/install.sh | sh
sudo apt-get install -y jq curl
```

Then log in to each service:

```sh
flyctl auth login
vercel login
gh auth login
stripe login    # opens browser + posts back an interactive session
```

## Prereqs — accounts you personally create

- **Domain registrar** (Namecheap / Cloudflare Registrar) — buy the domain.
- **Cloudflare** — add the domain, use Cloudflare's nameservers, generate an
  API token with `Zone:DNS:Edit` scope for the specific zone.
- **Fly.io** — free tier, add a credit card (launch runs ~$0.30/hr).
- **Vercel** — free tier is enough for launch.
- **Stripe** — start in test mode; verify identity for live mode later.
- **Postmark** (or Resend / SES) — for magic-link email delivery.
- **npmjs** — create the `@nanovm` scope; first publish is manual (see [`clients/typescript/README.md`](../../clients/typescript/README.md)).
- **PyPI** — register a "pending publisher" for the `nanovm` project pointing at [`python-publish.yml`](../../.github/workflows/python-publish.yml).

## First-run sequence

```sh
# 1. Copy the master env template and fill in every variable.
cp scripts/launch/.env.example scripts/launch/.env
$EDITOR scripts/launch/.env

# 2. Verify tools + env is complete. Errors on missing values.
./scripts/launch/preflight.sh

# 3. Set up Stripe products (Free/Pro/Team) + billing portal config.
#    Idempotent — safe to re-run. Prints the price IDs you paste
#    into .env's NANOVM_PLAN_TIERS.
./scripts/launch/stripe-setup.sh

# 4. Configure DNS at your registrar (Cloudflare API).
#    Points <domain> at Vercel, api.<domain> at Fly.io.
./scripts/launch/cloudflare-dns.sh

# 5. Deploy the marketing site to Vercel. Env vars propagate
#    automatically from .env.
./scripts/launch/vercel-deploy.sh

# 6. Deploy the control plane to Fly.io. Secrets propagate as
#    fly secrets set (never as env, per Fly's security model).
./scripts/launch/fly-deploy.sh

# 7. Add the Stripe webhook endpoint (needs api.<domain> live first).
#    Prints the whsec_... you paste back into .env, then re-run
#    fly-deploy.sh so the control plane picks it up.
./scripts/launch/stripe-webhook.sh

# 8. Smoke test the whole stack. Fails loudly on any missing piece.
./scripts/launch/smoke.sh
```

Total wall time from a clean laptop with the tools installed: ~45 min.

## Re-runnability

Every script here is **idempotent**. If step 5 fails midway, fix the
underlying issue and re-run — nothing bricks. Secrets already set are
skipped; existing DNS records with the right value are left alone;
Stripe products already created are looked up by name and re-used.

## Post-launch daily ops

```sh
# One-command status: DNS, Fly health, Vercel deploy, Stripe subscribes.
./scripts/launch/status.sh

# Tail the control plane's logs.
flyctl logs -a "$NANOVM_FLY_APP"

# See who's subscribed today.
stripe subscriptions list --limit 50
```

## What each script does in one line

| Script | Purpose | Reads | Writes / side-effects |
|---|---|---|---|
| `preflight.sh` | Verify tools + env file, fail-fast on missing values | `.env` | Prints checklist; exits non-zero on any gap |
| `stripe-setup.sh` | Create Free/Pro/Team products + prices + portal config | `.env` | Stripe products + prices; prints price IDs to paste |
| `cloudflare-dns.sh` | Point `<domain>` → Vercel, `api.<domain>` → Fly.io | `.env` | DNS records at Cloudflare (creates or updates) |
| `vercel-deploy.sh` | Build + deploy the marketing site | `.env` | Vercel deployment; sets `NEXT_PUBLIC_*` env vars |
| `fly-deploy.sh` | Build + deploy the control plane; sets secrets | `.env` | Fly.io app + machine + secrets |
| `stripe-webhook.sh` | Add webhook endpoint to Stripe (after api.<domain> lives) | `.env` | Stripe webhook endpoint; prints signing secret |
| `smoke.sh` | Verify every route + a signup round-trip | `.env` | Read-only smoke tests |
| `status.sh` | Health check across all four surfaces | `.env` | Read-only |

## Environment file

See [`.env.example`](.env.example) for every variable. **Never commit
your `.env`**. It's in `scripts/launch/.gitignore` for safety.
