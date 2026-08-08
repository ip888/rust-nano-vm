#!/usr/bin/env bash
# fly-deploy.sh — deploy the control plane to Fly.io.
#
# Creates the app if missing, plants every server-side secret from
# .env, then deploys the current ghcr.io image.
#
# Idempotent — re-run any time. Secrets that haven't changed are
# skipped (Fly compares by hash).
#
# Usage:
#   ./scripts/launch/fly-deploy.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
set -a
# shellcheck disable=SC1090,SC1091
source "${SCRIPT_DIR}/.env"
set +a

: "${FLY_API_TOKEN:?run preflight.sh first}"
: "${NANOVM_DOMAIN:?run preflight.sh first}"
: "${NANOVM_API_DOMAIN:?run preflight.sh first}"
: "${STRIPE_SECRET_KEY:?run preflight.sh first}"
: "${NANOVM_SMTP_URL:?run preflight.sh first}"
: "${NANOVM_SMTP_FROM:?run preflight.sh first}"
: "${NANOVM_SIGNUP_TOKEN:?run preflight.sh first}"
: "${NANOVM_OPERATOR_TOKEN:?run preflight.sh first}"
: "${NANOVM_FLY_APP:=nanovm-control-plane-prod}"
: "${NANOVM_FLY_REGION:=iad}"

export FLY_API_TOKEN

echo "== Fly.io deploy — app: ${NANOVM_FLY_APP} =="
echo

# ---- 1. Ensure the app exists --------------------------------------------

if ! flyctl apps show "${NANOVM_FLY_APP}" >/dev/null 2>&1; then
  echo "-- creating Fly app: ${NANOVM_FLY_APP}"
  flyctl apps create "${NANOVM_FLY_APP}" \
      --machines \
      --org "${NANOVM_FLY_ORG:-personal}"
else
  echo "-- app exists: ${NANOVM_FLY_APP}"
fi

# ---- 2. Volume for snapshot storage (10 GB) -------------------------------

VOLUME_NAME="nanovm_snapshots"
if ! flyctl volumes list -a "${NANOVM_FLY_APP}" 2>/dev/null | grep -q "${VOLUME_NAME}"; then
  echo "-- creating volume: ${VOLUME_NAME}"
  flyctl volumes create "${VOLUME_NAME}" \
      -a "${NANOVM_FLY_APP}" \
      --region "${NANOVM_FLY_REGION}" \
      --size 10 \
      --yes
else
  echo "-- volume exists: ${VOLUME_NAME}"
fi

# ---- 3. Secrets ----------------------------------------------------------

echo
echo "-- planting secrets"

# Build plan-tiers string from the three price IDs stripe-setup.sh
# emits. Fall back to sensible defaults if a price id is missing so
# the deploy doesn't hard-fail during first iteration.
PLAN_TIERS_STR=""
if [[ -n "${STRIPE_PRICE_ID_FREE:-}" ]]; then
  PLAN_TIERS_STR+="${STRIPE_PRICE_ID_FREE}=free:5"
fi
if [[ -n "${STRIPE_PRICE_ID_PRO:-}" ]]; then
  [[ -n "${PLAN_TIERS_STR}" ]] && PLAN_TIERS_STR+=","
  PLAN_TIERS_STR+="${STRIPE_PRICE_ID_PRO}=pro:100"
fi
if [[ -n "${STRIPE_PRICE_ID_TEAM:-}" ]]; then
  [[ -n "${PLAN_TIERS_STR}" ]] && PLAN_TIERS_STR+=","
  PLAN_TIERS_STR+="${STRIPE_PRICE_ID_TEAM}=team:500"
fi

# `flyctl secrets set` diffs against existing values by hash — no-op
# when unchanged. Deploys the machine to pick up new values in the
# same call.
flyctl secrets set -a "${NANOVM_FLY_APP}" --stage \
    NANOVM_API_TOKENS="${NANOVM_OPERATOR_TOKEN}" \
    NANOVM_SIGNUP_TOKEN="${NANOVM_SIGNUP_TOKEN}" \
    NANOVM_CORS_ORIGIN="https://${NANOVM_DOMAIN}" \
    STRIPE_SECRET_KEY="${STRIPE_SECRET_KEY}" \
    STRIPE_WEBHOOK_SIGNING_SECRET="${STRIPE_WEBHOOK_SIGNING_SECRET:-placeholder-run-stripe-webhook-sh}" \
    STRIPE_BILLING_PORTAL_RETURN_URL="https://${NANOVM_DOMAIN}/dashboard" \
    NANOVM_PLAN_TIERS="${PLAN_TIERS_STR}" \
    NANOVM_SMTP_URL="${NANOVM_SMTP_URL}" \
    NANOVM_SMTP_FROM="${NANOVM_SMTP_FROM}" \
    NANOVM_SIGNUP_VERIFY_URL="https://${NANOVM_DOMAIN}/signup/verify" \
    NANOVM_TOKEN_STORE_PATH="/data/tokens.json" \
    NANOVM_AUDIT_LOG="/data/audit.jsonl"

# ---- 4. Copy fly.toml to a launch-specific one ---------------------------

# Start from the live-demo template and override the app name +
# volume mount. `deploy/live-demo/fly/fly.toml` is a good baseline.
FLY_MANIFEST="${SCRIPT_DIR}/.fly.toml.generated"
sed -e "s/^app = .*/app = \"${NANOVM_FLY_APP}\"/" \
    -e "s/^primary_region = .*/primary_region = \"${NANOVM_FLY_REGION}\"/" \
    "${REPO_ROOT}/deploy/live-demo/fly/fly.toml" \
    > "${FLY_MANIFEST}"

# Ensure the volume mount is in the manifest.
if ! grep -q "\[mounts\]" "${FLY_MANIFEST}"; then
  cat >> "${FLY_MANIFEST}" <<MOUNT

[mounts]
  source = "${VOLUME_NAME}"
  destination = "/data"
MOUNT
fi

echo
echo "-- deploying image (this takes 3-5 min the first time)"
flyctl deploy \
    -a "${NANOVM_FLY_APP}" \
    -c "${FLY_MANIFEST}" \
    --strategy immediate \
    --wait-timeout 600

# ---- 5. Attach custom domain --------------------------------------------

echo
echo "-- attaching custom domain: ${NANOVM_API_DOMAIN}"
if ! flyctl certs list -a "${NANOVM_FLY_APP}" 2>/dev/null | grep -q "${NANOVM_API_DOMAIN}"; then
  flyctl certs add "${NANOVM_API_DOMAIN}" -a "${NANOVM_FLY_APP}"
else
  echo "   certificate already provisioned for ${NANOVM_API_DOMAIN}"
fi

echo
echo "== SUCCESS =="
echo
echo "Control plane is live at:"
echo "  https://${NANOVM_API_DOMAIN}/v1/health"
echo "  https://${NANOVM_FLY_APP}.fly.dev/v1/health   (Fly-direct)"
echo
echo "Tail logs:"
echo "  flyctl logs -a ${NANOVM_FLY_APP}"
echo
echo "next: ./scripts/launch/stripe-webhook.sh"
