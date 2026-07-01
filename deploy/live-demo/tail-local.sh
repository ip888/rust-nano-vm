#!/usr/bin/env bash
#
# Interleave two streams so you can watch the local demo work:
#
#   1. control-plane stdout — via `docker logs -f`, JSON RUST_LOG lines
#   2. audit JSONL          — via `docker exec … tail -F` on the audit volume
#
# Each line gets a coloured [log] / [audit] prefix so you can tell them
# apart. Ctrl-C to stop.

set -euo pipefail

CONTAINER="nanovm-demo-control-plane"

if ! docker ps --format '{{.Names}}' | grep -qx "$CONTAINER"; then
  echo "container $CONTAINER not running. Run ./up-local.sh first." >&2
  exit 1
fi

# Pretty-print with jq if present, otherwise raw pass-through.
if command -v jq >/dev/null 2>&1; then
  FMT=(jq --unbuffered -c \
    '{ts: .timestamp // .ts, lvl: .level // .lvl, msg: .message // .fields.message, span: .span.name, target}')
else
  FMT=(cat)
fi

PIDS=()
trap 'kill "${PIDS[@]}" 2>/dev/null || true; exit 0' INT TERM

echo "─── control-plane RUST_LOG (docker logs) ────────────────────"
# jq first so it sees clean JSON; then prefix. Prefixing first would
# feed jq `[log] {json}` which fails, and under `set -o pipefail` the
# whole script would exit.
( docker logs -f --tail 20 "$CONTAINER" 2>&1 \
    | "${FMT[@]}" \
    | sed -u 's/^/\x1b[35m[log]\x1b[0m /'
) &
PIDS+=("$!")

echo "─── /var/log/nanovm/audit.jsonl (docker exec tail) ──────────"
( docker exec "$CONTAINER" tail -F /var/log/nanovm/audit.jsonl 2>/dev/null \
    | "${FMT[@]}" \
    | sed -u 's/^/\x1b[33m[audit]\x1b[0m /'
) &
PIDS+=("$!")

wait
