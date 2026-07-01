#!/usr/bin/env bash
#
# Tear down the local-KVM demo. Stops all 3 docker containers and
# (unless --keep-audit is passed) removes the audit-log volume.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"

log() { printf '\033[36m➜\033[0m %s\n' "$*"; }

log "stopping compose stack"
# Pass throwaway env vars so compose doesn't warn about missing values
# while it's just trying to look up what to stop.
KVM_GID=0 NANOVM_API_TOKENS="" \
  docker compose -f ./compose/docker-compose.local.yml down --remove-orphans

if [[ "${1:-}" == "--keep-audit" ]]; then
  log "keeping audit-log volume (--keep-audit)"
else
  log "removing audit-log volume"
  KVM_GID=0 NANOVM_API_TOKENS="" \
    docker compose -f ./compose/docker-compose.local.yml down -v --remove-orphans 2>/dev/null || true
fi

printf '\n\033[32m✓\033[0m Local demo torn down.\n'
