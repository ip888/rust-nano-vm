#!/usr/bin/env bash
#
# Tear the whole live-KVM demo down.
#
# By default this leaves the Fly.io app in place (`flyctl apps
# destroy` is destructive and asks for the app name to confirm — we
# don't want to nuke a real prod app if someone reuses this script).
# Pass `--destroy-fly-app` to also delete the Fly app.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"

log() { printf '\033[36m➜\033[0m %s\n' "$*"; }

# ---- Stop docker compose --------------------------------------------
log "stopping Prometheus + Grafana"
docker compose -f ./compose/docker-compose.yml down --remove-orphans

# ---- Stop Fly machine -----------------------------------------------
APP="${NANOVM_LIVE_DEMO_APP:-}"
if [[ -z "$APP" && -f ./.env.local ]]; then
  # shellcheck disable=SC1091
  source ./.env.local
  APP="${NANOVM_LIVE_DEMO_APP:-}"
fi

if [[ -n "$APP" ]] && command -v flyctl >/dev/null 2>&1; then
  if [[ "${1:-}" == "--destroy-fly-app" ]]; then
    log "destroying Fly app: $APP"
    flyctl apps destroy "$APP" --yes || true
    rm -f ./.env.local ./compose/prometheus.yml
  else
    log "stopping Fly machines (app kept — pass --destroy-fly-app to delete)"
    # Stop every machine in the app so you stop paying for it.
    flyctl scale count 0 --app "$APP" --yes || true
  fi
fi

printf '\n\033[32m✓\033[0m Demo torn down.\n'
if [[ "${1:-}" != "--destroy-fly-app" ]]; then
  printf '   Fly app "%s" is stopped but still exists (no compute charges).\n' "$APP"
  printf '   Bring it back up with ./up.sh, or destroy with ./down.sh --destroy-fly-app\n'
fi
