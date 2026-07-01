#!/usr/bin/env bash
#
# Bring the whole live-KVM demo up:
#
#   1. deploy the control plane to Fly.io (real /dev/kvm)
#   2. render prometheus.yml pointing at the Fly hostname
#   3. `docker compose up -d` the Prometheus + Grafana stack
#   4. print the URLs
#
# Prereqs on your laptop:
#   - flyctl (https://fly.io/docs/hands-on/install-flyctl/) + `flyctl auth login`
#   - docker + docker compose (or docker-compose plugin)
#   - curl, openssl (or /dev/urandom fallback)

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"

log() { printf '\033[36m➜\033[0m %s\n' "$*"; }
die() { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

command -v docker >/dev/null 2>&1 || die "docker not on PATH"

# ---- Step 1: deploy control plane to Fly.io -------------------------
log "step 1/3: deploying control plane to Fly.io (real KVM)"
./fly/deploy.sh

# Reload the env file the deploy step wrote (tokens + hostname).
# shellcheck disable=SC1091
source ./.env.local

# ---- Step 2: render prometheus.yml with the real hostname -----------
log "step 2/3: rendering prometheus.yml → $NANOVM_LIVE_DEMO_HOST"
NANOVM_FLY_HOST="$NANOVM_LIVE_DEMO_HOST" \
  envsubst < ./compose/prometheus.yml.tpl > ./compose/prometheus.yml

# ---- Step 3: docker compose up --------------------------------------
log "step 3/3: starting Prometheus + Grafana (docker compose)"
docker compose -f ./compose/docker-compose.yml up -d

# Give Grafana a moment to import the dashboard, then print the URLs.
sleep 2

cat <<EOF

\033[32m✓\033[0m Live-KVM demo is running.

  \033[1mControl plane (real KVM on Fly.io):\033[0m
    Health:    https://$NANOVM_LIVE_DEMO_HOST/v1/health
    Metrics:   https://$NANOVM_LIVE_DEMO_HOST/metrics
    OpenAPI:   https://$NANOVM_LIVE_DEMO_HOST/openapi.json

  \033[1mLocal observability (your laptop):\033[0m
    Prometheus: http://localhost:9090
    Grafana:    http://localhost:3000/d/nanovm-overview

  \033[1mNext:\033[0m
    ./load.sh   # in one terminal — pumps realistic multi-org traffic
    ./tail.sh   # in another terminal — streams audit-log JSON

  Then open http://localhost:3000/d/nanovm-overview and watch the
  panels move in real time. Every fork counter tick, every 429 throttle,
  every warm-pool hit is a real KVM ioctl round-trip on a real Linux
  kernel on Fly.io — not a mock.

  \033[1mWhen done:\033[0m
    ./down.sh

EOF
