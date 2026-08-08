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

# health probe
health="$(curl -sS --max-time 5 "https://${NANOVM_API_DOMAIN}/v1/health" 2>/dev/null || echo '{}')"
if echo "${health}" | jq -e '.ok == true' > /dev/null 2>&1; then
  backend="$(echo "${health}" | jq -r '.backend')"
  uptime="$(echo "${health}" | jq -r '.uptime_secs')"
  ok "health" "backend=${backend} uptime=${uptime}s"
else
  bad "health" "endpoint returned no {ok:true} — control plane down?"
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
  # Count active subscriptions.
  active="$(curl -sS -u "${STRIPE_SECRET_KEY}:" \
      'https://api.stripe.com/v1/subscriptions?status=active&limit=100' \
      | jq -r '.data | length' 2>/dev/null || echo 0)"
  if [[ "${active}" -gt 0 ]]; then
    ok "active subscriptions" "${active}  🎉"
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
  answer="$(dig +short "${host}" | head -n1)"
  if [[ -n "${answer}" ]]; then
    ok "${host}" "→ ${answer}"
  else
    bad "${host}" "no A/CNAME"
  fi
done

echo
