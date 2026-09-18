#!/usr/bin/env bash
# smoke.sh — end-to-end smoke test the launched stack.
#
# Fails loudly on any missing piece so an operator knows exactly
# what to fix. Runs after the full deploy chain (preflight →
# stripe-setup → cloudflare-dns → vercel-deploy → fly-deploy →
# stripe-webhook).
#
# Read-only. Safe to run any time.
#
# Usage:
#   ./scripts/launch/smoke.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
set -a
# shellcheck disable=SC1090,SC1091
source "${SCRIPT_DIR}/.env"
set +a

: "${NANOVM_DOMAIN:?run preflight.sh first}"
: "${NANOVM_API_DOMAIN:?run preflight.sh first}"

c_green="$(printf '\033[32m')"; c_red="$(printf '\033[31m')"
c_yellow="$(printf '\033[33m')"; c_reset="$(printf '\033[0m')"
FAIL=0

ok()   { printf "  ${c_green}[✓]${c_reset} %s\n" "$*"; }
warn() { printf "  ${c_yellow}[!]${c_reset} %s\n" "$*"; }
bad()  { printf "  ${c_red}[×]${c_reset} %s\n" "$*"; FAIL=1; }

# Helper — check a URL returns the expected status.
http() {
  local desc="$1"
  local url="$2"
  local expected="${3:-200}"
  local status
  status="$(curl -sS -o /dev/null -w '%{http_code}' \
      --max-time 10 --location "${url}" 2>/dev/null || echo 000)"
  if [[ "${status}" == "${expected}" ]]; then
    ok "${desc}  (${status})  ${url}"
  else
    bad "${desc}  (got ${status}, expected ${expected})  ${url}"
  fi
}

echo
echo "== smoke test — nanovm.${NANOVM_DOMAIN} =="
echo

# ---- 1. DNS resolves both surfaces ---------------------------------------

echo "-- DNS"
if dig +short "${NANOVM_DOMAIN}" | grep -q .; then
  ok "${NANOVM_DOMAIN}  resolves"
else
  bad "${NANOVM_DOMAIN}  no DNS answer (cloudflare-dns.sh?)"
fi
if dig +short "${NANOVM_API_DOMAIN}" | grep -q .; then
  ok "${NANOVM_API_DOMAIN}  resolves"
else
  bad "${NANOVM_API_DOMAIN}  no DNS answer (cloudflare-dns.sh?)"
fi

# ---- 2. Marketing site --------------------------------------------------

echo
echo "-- marketing site (Vercel)"
http "landing"             "https://${NANOVM_DOMAIN}"
http "pricing"             "https://${NANOVM_DOMAIN}/pricing"
http "why-nanovm"          "https://${NANOVM_DOMAIN}/why-nanovm"
http "marketplace"         "https://${NANOVM_DOMAIN}/marketplace"
http "signup"              "https://${NANOVM_DOMAIN}/signup"
http "sitemap"             "https://${NANOVM_DOMAIN}/sitemap.xml"
http "robots"              "https://${NANOVM_DOMAIN}/robots.txt"
http "rss"                 "https://${NANOVM_DOMAIN}/rss.xml"
http "OG (landing)"        "https://${NANOVM_DOMAIN}/opengraph-image"
http "OG (pricing)"        "https://${NANOVM_DOMAIN}/pricing/opengraph-image"

# ---- 3. Control plane ---------------------------------------------------

echo
echo "-- control plane (Fly.io)"
http "health"              "https://${NANOVM_API_DOMAIN}/v1/health"
http "openapi"             "https://${NANOVM_API_DOMAIN}/openapi.json"
http "metrics"             "https://${NANOVM_API_DOMAIN}/metrics"
http "marketplace listing" "https://${NANOVM_API_DOMAIN}/v1/marketplace/snapshots"

# CORS preflight from the marketing origin — a mis-configured
# NANOVM_CORS_ORIGIN silently breaks every dashboard fetch.
echo
echo "-- CORS"
cors_origin="$(curl -sS -o /dev/null -w '%{header.access-control-allow-origin}' \
    -X OPTIONS \
    -H "Origin: https://${NANOVM_DOMAIN}" \
    -H "Access-Control-Request-Method: GET" \
    "https://${NANOVM_API_DOMAIN}/v1/health" 2>/dev/null || true)"
if [[ "${cors_origin}" == "https://${NANOVM_DOMAIN}" || "${cors_origin}" == "*" ]]; then
  ok "CORS  Access-Control-Allow-Origin = ${cors_origin}"
else
  bad "CORS  expected https://${NANOVM_DOMAIN}, got '${cors_origin}' (NANOVM_CORS_ORIGIN?)"
fi

# ---- 4. Signup gate is enforced (auth token required) ------------------

echo
echo "-- auth"
signup_status="$(curl -sS -o /dev/null -w '%{http_code}' \
    -X POST -H 'content-type: application/json' \
    --data '{"email":"smoke@example.com","org":"smoke"}' \
    "https://${NANOVM_API_DOMAIN}/v1/signup/request")"
if [[ "${signup_status}" == "401" || "${signup_status}" == "403" ]]; then
  ok "signup gate rejects no-token requests (${signup_status})"
elif [[ "${signup_status}" == "200" || "${signup_status}" == "202" ]]; then
  warn "signup is OPEN — NANOVM_SIGNUP_TOKEN not enforced. Confirm intentional."
else
  bad "signup returned unexpected status ${signup_status}"
fi

# ---- 5. Stripe billing plumbing (needs live subscription to be full) --

echo
echo "-- Stripe"
if [[ -n "${STRIPE_WEBHOOK_SIGNING_SECRET:-}" && "${STRIPE_WEBHOOK_SIGNING_SECRET}" != *placeholder* ]]; then
  ok "webhook signing secret present in .env"
else
  bad "STRIPE_WEBHOOK_SIGNING_SECRET missing — run stripe-webhook.sh + re-run fly-deploy.sh"
fi
if [[ -n "${STRIPE_PRICE_ID_PRO:-}" ]]; then
  ok "STRIPE_PRICE_ID_PRO  ${STRIPE_PRICE_ID_PRO}"
else
  bad "STRIPE_PRICE_ID_PRO empty — run stripe-setup.sh"
fi

# ---- 6. Report ----------------------------------------------------------

echo
if [[ ${FAIL} -eq 0 ]]; then
  printf "${c_green}== smoke tests pass — launch surface is live ==${c_reset}\n"
  echo
  echo "Next steps:"
  echo "  1. Mint the demo tenant token, add NEXT_PUBLIC_NANOVM_DEMO_TOKEN"
  echo "     to .env, re-run vercel-deploy.sh — LiveForkBenchmark flips"
  echo "     to Live."
  echo "  2. Fire the launch content in docs/launch/."
  echo "  3. Watch metrics per docs/launch/metrics.md."
  exit 0
else
  printf "${c_red}== smoke tests failed — fix the [×] items above ==${c_reset}\n"
  exit 1
fi
