#!/usr/bin/env bash
#
# Live-tail two streams side by side so you can see the platform work:
#
#   1. `flyctl logs`     — every RUST_LOG line the control plane emits
#                          (JSON, one line per event: HTTP request,
#                          fork decision, quota reject, warm-pool refill,
#                          IPC round-trip, KVM ioctl error, …)
#   2. audit JSONL       — one line per privileged API call:
#                            {"who":"acme","action":"fork","vm":42,…}
#
# Prints them interleaved with a header banner so you can tell which
# stream a line came from. Works in a single terminal — no tmux, no
# split panes required.
#
# Ctrl-C to stop.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
[[ -f "$HERE/.env.local" ]] || {
  echo "no .env.local — run ./up.sh first" >&2
  exit 1
}
# shellcheck disable=SC1091
source "$HERE/.env.local"

command -v flyctl >/dev/null 2>&1 || {
  echo "flyctl not on PATH" >&2
  exit 1
}

# Pretty-print with jq if present, otherwise raw.
if command -v jq >/dev/null 2>&1; then
  FMT=(jq --unbuffered -c \
    '{ts: .timestamp // .ts, lvl: .level // .lvl, msg: .message // .msg // .fields.message, span: .span.name, target}')
else
  FMT=(cat)
fi

PIDS=()
trap 'kill "${PIDS[@]}" 2>/dev/null || true; exit 0' INT TERM

echo "─── control-plane RUST_LOG (Fly.io) ───────────────────────────"
( flyctl logs --app "$NANOVM_LIVE_DEMO_APP" 2>/dev/null \
    | sed -u 's/^/\x1b[35m[log]\x1b[0m /' \
    | "${FMT[@]}"
) &
PIDS+=("$!")

echo "─── /var/log/nanovm/audit.jsonl (Fly.io) ──────────────────────"
( flyctl ssh console --app "$NANOVM_LIVE_DEMO_APP" \
    -C 'tail -F /var/log/nanovm/audit.jsonl' 2>/dev/null \
    | sed -u 's/^/\x1b[33m[audit]\x1b[0m /' \
    | "${FMT[@]}"
) &
PIDS+=("$!")

wait
