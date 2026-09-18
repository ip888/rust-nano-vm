#!/usr/bin/env bash
# preflight.sh — verify the launch toolkit + env are complete.
#
# Fail-fast on any missing tool, missing env variable, or malformed
# value BEFORE any subsequent script tries to make an irreversible
# state change on Fly / Vercel / Stripe / Cloudflare.
#
# Idempotent, read-only. Safe to run any time.
#
# Usage:
#   ./scripts/launch/preflight.sh
#
# Exit codes:
#   0 — all good, launch scripts safe to run
#   1 — one or more missing tools / envs; look for `[×]` lines
set -euo pipefail

# Locate + load .env (auto-loads for every downstream script too).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="${SCRIPT_DIR}/.env"

# Colour + status helpers — kept minimal so the output is grep-able.
c_green="$(printf '\033[32m')"
c_red="$(printf '\033[31m')"
c_yellow="$(printf '\033[33m')"
c_reset="$(printf '\033[0m')"
FAIL=0

ok()   { printf "  ${c_green}[✓]${c_reset} %s\n" "$*"; }
warn() { printf "  ${c_yellow}[!]${c_reset} %s\n" "$*"; }
bad()  { printf "  ${c_red}[×]${c_reset} %s\n" "$*"; FAIL=1; }

echo
echo "== preflight — nanovm production launch =="
echo

# ---- 1. .env file exists ---------------------------------------------------

if [[ ! -f "${ENV_FILE}" ]]; then
  bad ".env not found at ${ENV_FILE}"
  bad "run: cp scripts/launch/.env.example scripts/launch/.env  &&  \$EDITOR scripts/launch/.env"
  exit 1
fi
ok ".env present at ${ENV_FILE}"

# Auto-export every variable in .env for the rest of this script.
set -a
# shellcheck disable=SC1090
source "${ENV_FILE}"
set +a

# ---- 2. Required tools present --------------------------------------------

echo
echo "-- tools --"
for tool in flyctl vercel gh stripe jq curl openssl; do
  if command -v "${tool}" > /dev/null 2>&1; then
    ver="$("${tool}" --version 2>&1 | head -n1 || true)"
    ok "${tool}  ${ver}"
  else
    bad "${tool} not installed"
  fi
done

# ---- 3. Required env values ------------------------------------------------

echo
echo "-- required env --"
check_env() {
  local name="$1"
  local val="${!name:-}"
  local pattern="${2:-}"
  if [[ -z "${val}" ]]; then
    bad "${name} is empty"
    return
  fi
  if [[ -n "${pattern}" ]] && [[ ! "${val}" =~ ${pattern} ]]; then
    bad "${name} present but doesn't match /${pattern}/"
    return
  fi
  # Mask credential-ish values in the output.
  case "${name}" in
    *TOKEN*|*KEY*|*SECRET*|*PASSWORD*|*API*)
      ok "${name}  set (${#val} chars, masked)"
      ;;
    *)
      ok "${name}  ${val}"
      ;;
  esac
}

check_env NANOVM_DOMAIN            '^[a-z0-9.-]+\.[a-z]{2,}$'
check_env NANOVM_API_DOMAIN        '^[a-z0-9.-]+\.[a-z]{2,}$'
check_env CLOUDFLARE_API_TOKEN
check_env CLOUDFLARE_ZONE_ID       '^[a-f0-9]{16,}$'
check_env VERCEL_TOKEN
check_env FLY_API_TOKEN
check_env STRIPE_SECRET_KEY        '^sk_(test|live)_'
check_env NANOVM_SMTP_URL          '^smtps?://'
check_env NANOVM_SMTP_FROM         '^[^@]+@[a-z0-9.-]+\.[a-z]{2,}$'
check_env NANOVM_SIGNUP_TOKEN
check_env NANOVM_OPERATOR_TOKEN    '^[^:]+:[^@]+@(admin|developer|viewer)$'

echo
echo "-- optional env --"
for var in STRIPE_WEBHOOK_SIGNING_SECRET \
           STRIPE_PRICE_ID_FREE STRIPE_PRICE_ID_PRO STRIPE_PRICE_ID_TEAM \
           NEXT_PUBLIC_NANOVM_DEMO_TOKEN \
           NANOVM_SNAPSHOT_STORE_URL \
           GRAFANA_CLOUD_PROM_URL; do
  val="${!var:-}"
  if [[ -z "${val}" ]]; then
    warn "${var}  unset (fine for first-run — filled in by later scripts)"
  else
    ok "${var}  set"
  fi
done

# ---- 4. Warn on suspicious values -----------------------------------------

echo
echo "-- sanity --"
if [[ "${STRIPE_SECRET_KEY:-}" == sk_live_* ]]; then
  warn "STRIPE_SECRET_KEY is a LIVE key — every checkout will charge a real card. Confirm intentional."
fi
if [[ "${NANOVM_OPERATOR_TOKEN:-}" == *CHANGEME* ]]; then
  bad "NANOVM_OPERATOR_TOKEN still contains CHANGEME — replace it with a real random secret."
  bad "  generate: openssl rand -hex 32"
fi
if [[ ${#NANOVM_SIGNUP_TOKEN} -lt 32 ]]; then
  bad "NANOVM_SIGNUP_TOKEN is under 32 chars; too weak. Regenerate with `openssl rand -hex 32`."
fi

# ---- 5. Report -------------------------------------------------------------

echo
if [[ ${FAIL} -eq 0 ]]; then
  printf "${c_green}== ready to launch ==${c_reset}\n"
  echo "next: ./scripts/launch/stripe-setup.sh"
  exit 0
else
  printf "${c_red}== preflight failed — fix the [×] items above and re-run ==${c_reset}\n"
  exit 1
fi
