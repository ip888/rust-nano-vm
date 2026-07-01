#!/usr/bin/env bash
#
# Realistic multi-org traffic generator against the live-KVM control
# plane. Runs three concurrent "orgs" (acme, globex, initech) hitting
# the same endpoints at different rates so every dashboard panel has
# something to draw:
#
#   - `nanovm_forks_total{token="…"}`                    per-token forks
#   - `nanovm_forks_total_by_org{org="…"}`               per-org forks
#   - `nanovm_fork_quota_throttled_total_by_org{org=…}`  the 429 curve
#   - `nanovm_warm_pool_hits_total` / `_misses_total`    hit ratio
#   - `nanovm_fork_latency_ms_sum` / `_count`            p50 / p99
#
# Ctrl-C to stop; the script traps and kills its background workers.
#
# Sources ../.env.local for the Fly hostname + per-org tokens (both
# written by fly/deploy.sh).

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
[[ -f "$HERE/.env.local" ]] || {
  echo "no .env.local — run ./up.sh first" >&2
  exit 1
}
# shellcheck disable=SC1091
source "$HERE/.env.local"

BASE_URL="$NANOVM_LIVE_DEMO_BASE_URL"

# ---- Helpers --------------------------------------------------------
call() {
  # $1 method, $2 path, $3 token, $4 body (optional)
  local method="$1" path="$2" token="$3" body="${4:-}"
  if [[ -n "$body" ]]; then
    curl -sS -X "$method" "$BASE_URL$path" \
      -H "Authorization: Bearer $token" \
      -H "Content-Type: application/json" \
      -w '\n%{http_code}\n' \
      -d "$body"
  else
    curl -sS -X "$method" "$BASE_URL$path" \
      -H "Authorization: Bearer $token" \
      -w '\n%{http_code}\n'
  fi
}

# One end-to-end lifecycle per invocation: create → snapshot → fork
# child → exec → destroy child. Prints one line per fork with the
# HTTP status so you can see 429s scroll by.
lifecycle_once() {
  local org="$1" token="$2"
  local now vm_id snap_id fork_id fork_status
  now="$(date +%H:%M:%S)"

  # Create base VM.
  vm_id=$(curl -fsS -X POST "$BASE_URL/v1/vms" \
    -H "Authorization: Bearer $token" \
    -H "Content-Type: application/json" \
    -d '{"cpus":1,"memory_mib":128,"kernel":"vmlinux","rootfs":"rootfs.ext4"}' \
    | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2) || return 0

  # Snapshot it.
  snap_id=$(curl -fsS -X POST "$BASE_URL/v1/vms/$vm_id/snapshot" \
    -H "Authorization: Bearer $token" \
    | grep -o '"id":"[^"]*"' | cut -d'"' -f4) || return 0

  # Fork three times so we visibly move `forks_total` per iteration.
  for _ in 1 2 3; do
    fork_status=$(curl -sS -o /dev/null -w '%{http_code}' \
      -X POST "$BASE_URL/v1/snapshots/$snap_id/fork" \
      -H "Authorization: Bearer $token")
    printf '\033[36m[%s]\033[0m %-8s fork %s → HTTP %s\n' \
      "$now" "$org" "$snap_id" "$fork_status"
  done

  # Exec something in one fresh fork so `exec` panels move too.
  fork_id=$(curl -fsS -X POST "$BASE_URL/v1/snapshots/$snap_id/fork" \
    -H "Authorization: Bearer $token" \
    | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2) || return 0
  if [[ -n "$fork_id" ]]; then
    curl -sS -o /dev/null -X POST "$BASE_URL/v1/vms/$fork_id/exec" \
      -H "Authorization: Bearer $token" \
      -H "Content-Type: application/json" \
      -d '{"cmd":["/bin/echo","live-demo"],"timeout_ms":2000}' || true
    curl -sS -o /dev/null -X DELETE "$BASE_URL/v1/vms/$fork_id" \
      -H "Authorization: Bearer $token" || true
  fi
}

# One long-running worker per org — different sleep intervals so the
# per-org series diverge on the dashboard.
worker() {
  local org="$1" token="$2" sleep_s="$3"
  while true; do
    lifecycle_once "$org" "$token" || true
    sleep "$sleep_s"
  done
}

# ---- Trap cleanup ---------------------------------------------------
PIDS=()
cleanup() {
  echo
  echo "stopping workers..."
  for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
  exit 0
}
trap cleanup INT TERM

# ---- Preflight ------------------------------------------------------
echo "checking control plane at $BASE_URL"
BACKEND=$(curl -fsS "$BASE_URL/v1/health" \
  -H "Authorization: Bearer $ACME_TOKEN" \
  | grep -o '"backend":"[^"]*"' | cut -d'"' -f4 || echo unknown)
if [[ "$BACKEND" != "kvm" ]]; then
  echo "warn: backend reports \"$BACKEND\" (expected \"kvm\")" >&2
fi
echo "backend: $BACKEND"
echo

# ---- Launch workers -------------------------------------------------
echo "starting workers:"
echo "  acme    → fork ~every 1s   (will trip fork-quota → 429s)"
echo "  globex  → fork ~every 3s   (well within quota)"
echo "  initech → fork ~every 6s   (idle-ish)"
echo
echo "watch http://localhost:3000/d/nanovm-overview move."
echo "Ctrl-C to stop."
echo

worker acme    "$ACME_TOKEN"    1  &  PIDS+=("$!")
worker globex  "$GLOBEX_TOKEN"  3  &  PIDS+=("$!")
worker initech "$INITECH_TOKEN" 6  &  PIDS+=("$!")

wait
