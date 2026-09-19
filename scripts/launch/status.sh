#!/usr/bin/env bash
# status.sh — one-command daily-ops health check.
#
# Read-only. Prints a status table across DNS, Vercel, Fly, and
# Stripe. Green rows are healthy; yellow rows are warnings; red rows
# are things to actually go fix.
#
# Usage:
#   ./scripts/launch/status.sh
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

row() {
  local color="$1"; shift
  printf "  ${color}[%s]${c_reset} %-24s %s\n" "$1" "$2" "$3"
}
ok()   { row "${c_green}" "✓" "$1" "$2"; }
warn() { row "${c_yellow}" "!" "$1" "$2"; }
bad()  { row "${c_red}" "×" "$1" "$2"; }

echo
echo "== status — ${NANOVM_DOMAIN} =="
echo

# ---- Fly.io control plane -----------------------------------------------

echo "control plane"
if [[ -n "${FLY_API_TOKEN:-}" ]]; then
  status_json="$(FLY_API_TOKEN="${FLY_API_TOKEN}" \
      flyctl status -a "${NANOVM_FLY_APP:-nanovm-control-plane-prod}" --json 2>/dev/null || echo '{}')"
  machine_state="$(echo "${status_json}" | jq -r '.Machines[0].state // "unknown"')"
  case "${machine_state}" in
    started) ok "machine" "${machine_state}" ;;
    stopped|suspended) warn "machine" "${machine_state}" ;;
    *) bad "machine" "${machine_state}" ;;
  esac
else
  warn "machine" "FLY_API_TOKEN unset; skipping Fly status check"
fi

# health probe — /healthz is the unauthenticated liveness endpoint;
# /v1/health is behind the bearer-token auth middleware. Detailed
# JSON is only available on /v1/health, so use the operator bearer
# when NANOVM_OPERATOR_TOKEN is present, otherwise fall back to
# /healthz for a plain-text ok probe.
if [[ -n "${NANOVM_OPERATOR_TOKEN:-}" ]]; then
  health="$(curl -sS --max-time 5 \
      -H "Authorization: Bearer ${NANOVM_OPERATOR_TOKEN#*:}" \
      -H "X-Org-Id: ${NANOVM_OPERATOR_TOKEN%%:*}" \
      "https://${NANOVM_API_DOMAIN}/v1/health" 2>/dev/null || echo '{}')"
  if echo "${health}" | jq -e '.ok == true' > /dev/null 2>&1; then
    backend="$(echo "${health}" | jq -r '.backend')"
    uptime="$(echo "${health}" | jq -r '.uptime_secs')"
    ok "health" "backend=${backend} uptime=${uptime}s"
  else
    bad "health" "endpoint returned no {ok:true} — control plane down?"
  fi
else
  # Fallback: unauthenticated /healthz. Just checks reachability.
  healthz_body="$(curl -sS --max-time 5 "https://${NANOVM_API_DOMAIN}/healthz" 2>/dev/null || echo '')"
  if [[ "${healthz_body}" == "ok" ]]; then
    ok "health" "/healthz  ok"
  else
    bad "health" "endpoint returned '${healthz_body}' — control plane down?"
  fi
fi

# ---- Marketing site ------------------------------------------------------

echo
echo "marketing"
landing_status="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 5 \
    "https://${NANOVM_DOMAIN}" 2>/dev/null || echo 000)"
if [[ "${landing_status}" == "200" ]]; then
  ok "landing" "https://${NANOVM_DOMAIN}  200"
else
  bad "landing" "status=${landing_status}"
fi

# ---- Stripe billing ------------------------------------------------------

echo
echo "billing"
if [[ -n "${STRIPE_SECRET_KEY:-}" ]]; then
  # Count active subscriptions. Stripe caps a single list call at 100
  # results; follow the has_more cursor so the count stays accurate
  # past the first page.
  total=0
  starting_after=""
  while :; do
    query="status=active&limit=100"
    [[ -n "${starting_after}" ]] && query+="&starting_after=${starting_after}"
    page="$(curl -sS -u "${STRIPE_SECRET_KEY}:" \
        "https://api.stripe.com/v1/subscriptions?${query}" 2>/dev/null || echo '{}')"
    page_count="$(echo "${page}" | jq -r '.data | length' 2>/dev/null || echo 0)"
    total=$((total + page_count))
    has_more="$(echo "${page}" | jq -r '.has_more // false')"
    if [[ "${has_more}" != "true" || "${page_count}" -eq 0 ]]; then
      break
    fi
    starting_after="$(echo "${page}" | jq -r '.data[-1].id')"
  done
  if [[ "${total}" -gt 0 ]]; then
    ok "active subscriptions" "${total}  🎉"
  else
    warn "active subscriptions" "0 — first paying customer not yet in"
  fi
else
  warn "stripe" "STRIPE_SECRET_KEY unset"
fi

# ---- DNS ----------------------------------------------------------------

echo
echo "DNS"
for host in "${NANOVM_DOMAIN}" "${NANOVM_API_DOMAIN}"; do
  # `dig +short` returning non-zero under `set -euo pipefail` would
  # abort the entire report before it finishes rendering. Treat a
  # transient failure as an empty answer and mark the row red.
  answer="$(dig +short "${host}" 2>/dev/null | head -n1 || true)"
  if [[ -n "${answer}" ]]; then
    ok "${host}" "→ ${answer}"
  else
    bad "${host}" "no A/CNAME"
  fi
done

# ---- Vercel deployment (marketing) --------------------------------------
#
# The landing-URL curl above proves the domain resolves and Vercel's
# CDN returns 200; it does NOT confirm that Vercel's own
# deployments/promotion state is healthy (a stale cache, a paused
# project, or a rolled-back deploy could all still 200). If a
# VERCEL_TOKEN is present, check the API directly for the latest
# READY promoted-to-production deployment.

if [[ -n "${VERCEL_TOKEN:-}" ]]; then
  echo
  echo "Vercel"
  project_slug="${VERCEL_PROJECT:-nanovm-web}"
  latest="$(curl -sS -H "Authorization: Bearer ${VERCEL_TOKEN}" \
      "https://api.vercel.com/v6/deployments?projectId=${project_slug}&target=production&limit=1" \
      2>/dev/null || echo '{}')"
  latest_state="$(echo "${latest}" | jq -r '.deployments[0].readyState // "unknown"')"
  latest_url="$(echo "${latest}" | jq -r '.deployments[0].url // ""')"
  case "${latest_state}" in
    READY) ok "latest deployment" "${latest_state}  ${latest_url}" ;;
    BUILDING|QUEUED|INITIALIZING) warn "latest deployment" "${latest_state}  ${latest_url}" ;;
    *) bad "latest deployment" "${latest_state}  ${latest_url}" ;;
  esac
fi

echo
