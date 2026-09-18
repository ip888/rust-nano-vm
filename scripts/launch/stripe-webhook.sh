#!/usr/bin/env bash
# stripe-webhook.sh — register the Stripe webhook endpoint against
# the deployed control plane, and print the signing secret to paste
# back into .env.
#
# Runs AFTER fly-deploy.sh (needs api.<domain> to be resolvable —
# Stripe verifies the endpoint at registration time).
#
# Idempotent — updates existing webhook endpoints in place instead
# of creating duplicates.
#
# Usage:
#   ./scripts/launch/stripe-webhook.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
set -a
# shellcheck disable=SC1090,SC1091
source "${SCRIPT_DIR}/.env"
set +a

: "${STRIPE_SECRET_KEY:?run preflight.sh first}"
: "${NANOVM_API_DOMAIN:?run preflight.sh first}"

WEBHOOK_URL="https://${NANOVM_API_DOMAIN}/v1/stripe/webhook"

# Every event we handle in `crates/control-plane/src/billing.rs`:
# — customer.subscription.created / updated / deleted   → plan changes
# — invoice.payment_succeeded / failed                  → dunning state
# — checkout.session.completed                          → first-charge
EVENTS=(
  "customer.subscription.created"
  "customer.subscription.updated"
  "customer.subscription.deleted"
  "invoice.payment_succeeded"
  "invoice.payment_failed"
  "checkout.session.completed"
)

echo "== Stripe webhook registration =="
echo "  url:    ${WEBHOOK_URL}"
echo "  events: ${#EVENTS[@]} configured"
echo

# Look up an existing endpoint at the same URL.
existing_id="$(curl -sS -u "${STRIPE_SECRET_KEY}:" \
    https://api.stripe.com/v1/webhook_endpoints?limit=100 \
    | jq -r ".data[] | select(.url == \"${WEBHOOK_URL}\") | .id" \
    | head -n1)"

# Build the --data-urlencode args for the event list.
event_args=()
for i in "${!EVENTS[@]}"; do
  event_args+=(--data-urlencode "enabled_events[${i}]=${EVENTS[$i]}")
done

if [[ -n "${existing_id}" ]]; then
  echo "-- updating existing endpoint: ${existing_id}"
  curl -sS -u "${STRIPE_SECRET_KEY}:" \
      "https://api.stripe.com/v1/webhook_endpoints/${existing_id}" \
      "${event_args[@]}" \
      | jq -r '"   updated:  " + .id'
  echo
  echo "!! Stripe does NOT re-expose the signing secret on updates."
  echo "!! If you didn't record it earlier, delete the endpoint from"
  echo "!! the Stripe Dashboard and re-run this script to get a fresh"
  echo "!! secret."
  exit 0
fi

echo "-- creating new endpoint"
response="$(curl -sS -u "${STRIPE_SECRET_KEY}:" \
    https://api.stripe.com/v1/webhook_endpoints \
    --data-urlencode "url=${WEBHOOK_URL}" \
    "${event_args[@]}")"

endpoint_id="$(echo "${response}" | jq -r '.id // ""')"
signing_secret="$(echo "${response}" | jq -r '.secret // ""')"

if [[ -z "${endpoint_id}" || -z "${signing_secret}" ]]; then
  echo "!! creation failed. Raw response:" >&2
  echo "${response}" | jq . >&2
  exit 1
fi

echo "   created:  ${endpoint_id}"
echo
echo "== SUCCESS =="
echo
echo "Paste this into scripts/launch/.env, replacing the placeholder:"
echo
echo "  STRIPE_WEBHOOK_SIGNING_SECRET=${signing_secret}"
echo
echo "Then re-run fly-deploy.sh so the control plane picks it up:"
echo
echo "  ./scripts/launch/fly-deploy.sh"
echo
echo "next: ./scripts/launch/smoke.sh"
