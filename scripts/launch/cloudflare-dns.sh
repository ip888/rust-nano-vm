#!/usr/bin/env bash
# cloudflare-dns.sh — configure DNS for the launch.
#
# Points:
#   <domain>              → Vercel (CNAME → cname.vercel-dns.com)
#   api.<domain>          → Fly.io (CNAME → <app>.fly.dev)
#
# Idempotent — updates existing records to the right value, creates
# missing ones. Records that already point at the right target are
# left alone.
#
# Usage:
#   ./scripts/launch/cloudflare-dns.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
set -a
# shellcheck disable=SC1090,SC1091
source "${SCRIPT_DIR}/.env"
set +a

: "${CLOUDFLARE_API_TOKEN:?run preflight.sh first}"
: "${CLOUDFLARE_ZONE_ID:?run preflight.sh first}"
: "${NANOVM_DOMAIN:?run preflight.sh first}"
: "${NANOVM_API_DOMAIN:?run preflight.sh first}"
: "${NANOVM_FLY_APP:=nanovm-control-plane-prod}"

CF_API="https://api.cloudflare.com/client/v4"
AUTH="Authorization: Bearer ${CLOUDFLARE_API_TOKEN}"

# ---- helper: assert a DNS record exists at the right value ---------------

# Assert that a Cloudflare API response's `.success` flag is true;
# print `.errors[].message` and abort otherwise. Called after every
# create/update/delete so a bad token, zone, or content isn't silently
# rendered as an "id=null" success line.
cf_assert() {
  local response="$1"
  local ctx="$2"
  if [[ "$(echo "${response}" | jq -r '.success')" != "true" ]]; then
    echo "!! Cloudflare API failure — ${ctx}" >&2
    echo "${response}" | jq -r '.errors[]? | "   \(.code) \(.message)"' >&2
    exit 1
  fi
}

ensure_record() {
  local rec_type="$1"     # CNAME | A
  local rec_name="$2"     # nanovm.example.com  (apex uses the bare domain)
  local rec_content="$3"  # cname.vercel-dns.com | <fly-app>.fly.dev
  local proxied="${4:-true}"

  echo "-- ensure ${rec_type} ${rec_name} → ${rec_content}"

  # Look up existing record. NOTE: no `type=` filter — we want to see
  # ANY record at this name so an old A/AAAA record blocking a CNAME
  # rewrite is visible and gets replaced, not silently coexisted with.
  local existing
  existing="$(curl -sS -H "${AUTH}" \
      "${CF_API}/zones/${CLOUDFLARE_ZONE_ID}/dns_records?name=${rec_name}" \
      2>/dev/null || echo '{}')"
  cf_assert "${existing}" "lookup ${rec_name}"
  local existing_id existing_type existing_content existing_proxied
  existing_id="$(echo "${existing}" | jq -r '.result[0].id // ""')"
  existing_type="$(echo "${existing}" | jq -r '.result[0].type // ""')"
  existing_content="$(echo "${existing}" | jq -r '.result[0].content // ""')"
  existing_proxied="$(echo "${existing}" | jq -r '.result[0].proxied // false')"

  local body
  body="$(jq -n \
      --arg type "${rec_type}" \
      --arg name "${rec_name}" \
      --arg content "${rec_content}" \
      --argjson proxied "${proxied}" \
      '{type: $type, name: $name, content: $content, ttl: 1, proxied: $proxied}')"

  if [[ -z "${existing_id}" ]]; then
    local response
    response="$(curl -sS -X POST -H "${AUTH}" -H "Content-Type: application/json" \
        "${CF_API}/zones/${CLOUDFLARE_ZONE_ID}/dns_records" \
        --data "${body}")"
    cf_assert "${response}" "create ${rec_name}"
    echo "   [created] id=$(echo "${response}" | jq -r '.result.id')"
  elif [[ "${existing_type}" == "${rec_type}" \
        && "${existing_content}" == "${rec_content}" \
        && "${existing_proxied}" == "${proxied}" ]]; then
    echo "   [ok] already correct (id=${existing_id})"
  else
    local response
    response="$(curl -sS -X PUT -H "${AUTH}" -H "Content-Type: application/json" \
        "${CF_API}/zones/${CLOUDFLARE_ZONE_ID}/dns_records/${existing_id}" \
        --data "${body}")"
    cf_assert "${response}" "update ${rec_name}"
    echo "   [updated] was: ${existing_type} ${existing_content} → ${rec_type} $(echo "${response}" | jq -r '.result.content')"
  fi
}

echo "== Cloudflare DNS setup =="
echo "  domain:     ${NANOVM_DOMAIN}"
echo "  api:        ${NANOVM_API_DOMAIN}"
echo "  zone:       ${CLOUDFLARE_ZONE_ID}"
echo

# Vercel uses `cname.vercel-dns.com` for both apex (via CNAME flattening,
# which Cloudflare supports natively) and www variants.
ensure_record CNAME "${NANOVM_DOMAIN}"     "cname.vercel-dns.com"    true
ensure_record CNAME "www.${NANOVM_DOMAIN}" "cname.vercel-dns.com"    true

# api.<domain> → Fly.io app hostname.
# Proxied=false because Fly manages its own TLS cert for the app
# hostname; running through Cloudflare's proxy would double-terminate.
ensure_record CNAME "${NANOVM_API_DOMAIN}" "${NANOVM_FLY_APP}.fly.dev" false

echo
echo "== SUCCESS =="
echo
echo "DNS propagation typically completes inside 60 seconds on"
echo "Cloudflare. Verify:"
echo "  dig +short ${NANOVM_DOMAIN}"
echo "  dig +short ${NANOVM_API_DOMAIN}"
echo
echo "next: ./scripts/launch/vercel-deploy.sh"
