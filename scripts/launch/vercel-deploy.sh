#!/usr/bin/env bash
# vercel-deploy.sh — build + deploy the marketing site to Vercel.
#
# Sets every NEXT_PUBLIC_* env var from .env on the Vercel project
# (idempotent — updates existing values in place) then triggers a
# production deploy.
#
# Idempotent — safe to re-run.
#
# Usage:
#   ./scripts/launch/vercel-deploy.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
set -a
# shellcheck disable=SC1090,SC1091
source "${SCRIPT_DIR}/.env"
set +a

: "${VERCEL_TOKEN:?run preflight.sh first}"
: "${NANOVM_DOMAIN:?run preflight.sh first}"
: "${NANOVM_API_DOMAIN:?run preflight.sh first}"
: "${VERCEL_PROJECT:=nanovm-web}"

echo "== Vercel deploy — project: ${VERCEL_PROJECT} =="
echo

cd "${REPO_ROOT}/web"

# ---- link (idempotent — no-op if already linked) --------------------------

if [[ ! -f .vercel/project.json ]]; then
  echo "-- vercel link (first-run)"
  vercel link --yes \
      --token "${VERCEL_TOKEN}" \
      --project "${VERCEL_PROJECT}" \
      --scope "${VERCEL_ORG:-$(vercel teams ls --token "${VERCEL_TOKEN}" | awk 'NR==1{print $1}')}"
else
  echo "-- vercel link (already present)"
fi

# ---- env vars (idempotent — remove+add is the only reliable path) --------

set_env() {
  local key="$1"
  local val="$2"
  # `vercel env rm` errors if the var doesn't exist yet — that's fine.
  vercel env rm "${key}" production --token "${VERCEL_TOKEN}" --yes 2>/dev/null || true
  printf '%s' "${val}" | vercel env add "${key}" production \
      --token "${VERCEL_TOKEN}"
}

echo
echo "-- env vars"
set_env NEXT_PUBLIC_NANOVM_API_URL "https://${NANOVM_API_DOMAIN}"
set_env NEXT_PUBLIC_NANOVM_WEB_ORIGIN "https://${NANOVM_DOMAIN}"

# Optional demo tenant (flips LiveForkBenchmark to Live mode when set).
if [[ -n "${NEXT_PUBLIC_NANOVM_DEMO_TOKEN:-}" ]]; then
  set_env NEXT_PUBLIC_NANOVM_DEMO_URL "https://${NANOVM_API_DOMAIN}"
  set_env NEXT_PUBLIC_NANOVM_DEMO_TOKEN "${NEXT_PUBLIC_NANOVM_DEMO_TOKEN}"
  set_env NEXT_PUBLIC_NANOVM_DEMO_MARKETPLACE_NAME "${NEXT_PUBLIC_NANOVM_DEMO_MARKETPLACE_NAME:-python-3.12-minimal}"
fi

# ---- production deploy ---------------------------------------------------

echo
echo "-- deploy (production)"
deploy_url="$(vercel --prod --yes --token "${VERCEL_TOKEN}" 2>&1 | tail -n1)"
echo "   deployed:  ${deploy_url}"

# ---- attach custom domain -----------------------------------------------

echo
echo "-- attach custom domain: ${NANOVM_DOMAIN}"
# `vercel domains add` is idempotent — no-ops if the domain is already attached
# to the project.
vercel domains add "${NANOVM_DOMAIN}" "${VERCEL_PROJECT}" \
    --token "${VERCEL_TOKEN}" 2>&1 | grep -Ev '^$' || true

echo
echo "== SUCCESS =="
echo
echo "The marketing site is live at:"
echo "  https://${NANOVM_DOMAIN}   (Vercel edge, Cloudflare-proxied)"
echo "  https://${deploy_url}       (Vercel-direct deploy URL)"
echo
echo "next: ./scripts/launch/fly-deploy.sh"
