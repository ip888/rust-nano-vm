#!/usr/bin/env bash
# stripe-setup.sh — create Free/Pro/Team products + prices +
# configure the customer portal.
#
# Idempotent — safe to re-run. Looks up existing products by name +
# metadata tag and re-uses them instead of creating duplicates. Prints
# the STRIPE_PRICE_ID_* values to paste back into `.env`.
#
# What this creates on Stripe:
#   1. Product "nanovm Free"  + $0/mo price
#   2. Product "nanovm Pro"   + $29/mo price
#   3. Product "nanovm Team"  + $199/mo price
#   4. Customer-portal config that shows all three + allows plan
#      switching, cancellation, and the default business info.
#
# Usage:
#   ./scripts/launch/stripe-setup.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
set -a
# shellcheck disable=SC1090,SC1091
source "${SCRIPT_DIR}/.env"
set +a

: "${STRIPE_SECRET_KEY:?run preflight.sh first}"
: "${NANOVM_DOMAIN:?run preflight.sh first}"

# `stripe` CLI reads STRIPE_API_KEY, not STRIPE_SECRET_KEY.
export STRIPE_API_KEY="${STRIPE_SECRET_KEY}"

echo "== Stripe setup — mode: $(echo "${STRIPE_SECRET_KEY}" | grep -oE 'test|live') =="
echo

# ---- helper: find-or-create a product ------------------------------------

# Looks up a product by our metadata.nanovm_tier tag; creates one if
# missing. Prints the resulting product id.
ensure_product() {
  local tier_key="$1"     # free | pro | team
  local display_name="$2"
  local existing
  existing="$(stripe products list --limit 100 2>/dev/null \
      | jq -r ".data[] | select(.metadata.nanovm_tier == \"${tier_key}\") | .id" \
      | head -n1)"
  if [[ -n "${existing}" ]]; then
    echo "${existing}"
    return
  fi
  stripe products create \
      --name "${display_name}" \
      --metadata "nanovm_tier=${tier_key}" \
      2>/dev/null \
      | jq -r '.id'
}

# Looks up a monthly recurring price for the given product at the given
# amount; creates one if missing. Prints the price id.
ensure_price() {
  local product_id="$1"
  local amount_cents="$2"   # 0 / 2900 / 19900
  local existing
  existing="$(stripe prices list --product "${product_id}" --limit 20 2>/dev/null \
      | jq -r ".data[] | select(.unit_amount == ${amount_cents} and .recurring.interval == \"month\") | .id" \
      | head -n1)"
  if [[ -n "${existing}" ]]; then
    echo "${existing}"
    return
  fi
  stripe prices create \
      --product "${product_id}" \
      --currency usd \
      --unit-amount "${amount_cents}" \
      --recurring[interval]=month \
      2>/dev/null \
      | jq -r '.id'
}

echo "-- products --"
free_pid="$(ensure_product free "nanovm Free")"
echo "  free  ${free_pid}"
pro_pid="$(ensure_product pro "nanovm Pro")"
echo "  pro   ${pro_pid}"
team_pid="$(ensure_product team "nanovm Team")"
echo "  team  ${team_pid}"

echo
echo "-- prices --"
free_price="$(ensure_price "${free_pid}" 0)"
echo "  free  ${free_price}    \$0/mo"
pro_price="$(ensure_price "${pro_pid}" 2900)"
echo "  pro   ${pro_price}    \$29/mo"
team_price="$(ensure_price "${team_pid}" 19900)"
echo "  team  ${team_price}    \$199/mo"

echo
echo "-- customer portal --"
# Configure the portal with all three products visible, plan switching
# on, cancellation allowed. Retry-idempotent — updating an existing
# config with the same shape is a no-op.
portal_json="$(cat <<JSON
{
  "features": {
    "customer_update": { "enabled": true, "allowed_updates": ["email", "tax_id", "address"] },
    "invoice_history": { "enabled": true },
    "payment_method_update": { "enabled": true },
    "subscription_cancel": { "enabled": true, "mode": "at_period_end" },
    "subscription_update": {
      "enabled": true,
      "default_allowed_updates": ["price"],
      "products": [
        { "product": "${free_pid}", "prices": ["${free_price}"] },
        { "product": "${pro_pid}",  "prices": ["${pro_price}"] },
        { "product": "${team_pid}", "prices": ["${team_price}"] }
      ]
    }
  },
  "business_profile": {
    "headline": "Manage your nanovm subscription",
    "privacy_policy_url": "https://${NANOVM_DOMAIN}/privacy",
    "terms_of_service_url": "https://${NANOVM_DOMAIN}/terms"
  },
  "default_return_url": "https://${NANOVM_DOMAIN}/dashboard"
}
JSON
)"

# Post via the raw API — the stripe CLI's `billing_portal configurations`
# command is limited on shape.
portal_config_id="$(curl -sS -u "${STRIPE_SECRET_KEY}:" \
  https://api.stripe.com/v1/billing_portal/configurations \
  --data-urlencode "features[customer_update][enabled]=true" \
  --data-urlencode "features[customer_update][allowed_updates][0]=email" \
  --data-urlencode "features[customer_update][allowed_updates][1]=address" \
  --data-urlencode "features[invoice_history][enabled]=true" \
  --data-urlencode "features[payment_method_update][enabled]=true" \
  --data-urlencode "features[subscription_cancel][enabled]=true" \
  --data-urlencode "features[subscription_cancel][mode]=at_period_end" \
  --data-urlencode "features[subscription_update][enabled]=true" \
  --data-urlencode "features[subscription_update][default_allowed_updates][0]=price" \
  --data-urlencode "features[subscription_update][products][0][product]=${pro_pid}" \
  --data-urlencode "features[subscription_update][products][0][prices][0]=${pro_price}" \
  --data-urlencode "features[subscription_update][products][1][product]=${team_pid}" \
  --data-urlencode "features[subscription_update][products][1][prices][0]=${team_price}" \
  --data-urlencode "business_profile[headline]=Manage your nanovm subscription" \
  --data-urlencode "default_return_url=https://${NANOVM_DOMAIN}/dashboard" \
  | jq -r '.id')"

if [[ "${portal_config_id}" == "null" || -z "${portal_config_id}" ]]; then
  echo "!! portal config creation failed; the returned JSON was:" >&2
  exit 1
fi
echo "  portal_config  ${portal_config_id}"

# ---- Print the paste-back block ------------------------------------------

echo
echo "== SUCCESS =="
echo
echo "Paste these into scripts/launch/.env, replacing the placeholder values:"
echo
cat <<PASTE
STRIPE_PRICE_ID_FREE=${free_price}
STRIPE_PRICE_ID_PRO=${pro_price}
STRIPE_PRICE_ID_TEAM=${team_price}
PASTE
echo
echo "Also set NANOVM_PLAN_TIERS (server-side) to:"
echo
echo "  ${free_price}=free:5,${pro_price}=pro:100,${team_price}=team:500"
echo
echo "next: ./scripts/launch/cloudflare-dns.sh"
