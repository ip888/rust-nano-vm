#!/bin/sh
# Orderly launcher for the seven Petclinic microservices in a single
# nanovm guest.
#
# Sequence:
#   1. config-server      (:8888) — must be up before anything else
#   2. discovery-server   (:8761) — must be up before app services
#   3. admin-server       (:9090) — cosmetic ops UI, blocking so its
#                                    Eureka registration is timely
#   4. api-gateway + customers + vets + visits — launched in parallel
#      (Eureka handles their registration; nothing else waits on them
#      by boot order, only by Eureka registration count)
#
# After all seven JVMs are launched, `exec` hands off to
# `warmup.sh`, which waits until all seven are registered with Eureka
# and reachable behind api-gateway, then emits the ready marker and
# `wait`s so PID 1 exits with the last JVM (see M1 rootfs for the
# panic-on-exit contract).
#
# ## Kernel-cmdline knobs
#
# `NANOVM_TIER=all` (default) — this launcher runs every service in
#     this guest. Suitable for single-guest demos.
#
# `NANOVM_TIER=infra` — starts only config-server + discovery-server +
#     admin-server. Reserved for the future virtio-net-bridge PR that
#     splits the stack across two guests.
#
# `NANOVM_TIER=app` — starts only api-gateway + customers + vets +
#     visits. Assumes an infra tier is reachable at
#     `http://nanovm-infra:8888` and `http://nanovm-infra:8761`
#     (deterministic hostnames that a future host-side `/etc/hosts`
#     injection will supply). Not exercised by M2 PR #1 alone —
#     lands with the virtio-net PR.

set -eu

# Parse NANOVM_TIER from /proc/cmdline. Defaults to `all` when absent
# or the file is unmountable.
NANOVM_TIER="all"
if [ -r /proc/cmdline ]; then
    for tok in $(cat /proc/cmdline); do
        case "$tok" in
            NANOVM_TIER=*) NANOVM_TIER="${tok#NANOVM_TIER=}" ;;
        esac
    done
fi
echo "[tier-launcher] NANOVM_TIER=${NANOVM_TIER}" >&2

JDK="/opt/jdk-21/bin/java"
JARS="/opt/petclinic-ms"
mkdir -p /var/log/petclinic-ms

# Heap sizes tuned to keep the total under 2 GiB on the recommended
# 4-GiB-guest deployment: 512 MiB for config (Config Server holds all
# service configs in memory), 384 for Eureka, 256 for admin/gateway
# and the three business services. Boots run fine on smaller heaps
# but the fork-many phase benefits from steady GC state.
run_jvm() {
    name="$1"
    heap="$2"
    port_arg="$3"
    extra="${4:-}"
    log="/var/log/petclinic-ms/${name}.log"
    echo "[tier-launcher] starting ${name} (Xmx${heap} ${port_arg})" >&2
    # `-Dspring.profiles.active=default` forces the in-jar
    # application.yml (no Docker-profile hostname overrides). All
    # services then default to `localhost` for their Config Server /
    # Eureka lookups — which is exactly what we want inside a single
    # guest.
    "${JDK}" \
        -Xmx"${heap}" \
        -Xshare:auto \
        -XX:+UseZGC \
        --add-opens java.base/java.lang=ALL-UNNAMED \
        -Dspring.profiles.active=default \
        ${extra} \
        -jar "${JARS}/${name}.jar" ${port_arg} \
        > "${log}" 2>&1 &
    printf '%s' "$!"
}

# Wait for a given health URL to return 200. Bounded so a broken
# config server doesn't wedge the boot indefinitely.
wait_health() {
    name="$1"
    url="$2"
    max="${3:-90}"
    echo "[tier-launcher] waiting up to ${max}s for ${name} health at ${url}" >&2
    elapsed=0
    while [ "$elapsed" -lt "$max" ]; do
        status="$(curl -sS -o /dev/null -w '%{http_code}' \
                       --max-time 3 "$url" 2>/dev/null || echo 000)"
        if [ "$status" = "200" ]; then
            echo "[tier-launcher] ${name} healthy after ${elapsed}s" >&2
            return 0
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done
    echo "[tier-launcher] FATAL: ${name} never healthy within ${max}s" >&2
    tail -n 200 "/var/log/petclinic-ms/${name}.log" >&2 || true
    exit 1
}

# ---- infra tier: config → eureka → admin -----------------------------
INFRA_PIDS=""
if [ "$NANOVM_TIER" = "all" ] || [ "$NANOVM_TIER" = "infra" ]; then
    CFG_PID="$(run_jvm config-server 512m "--server.port=8888")"
    INFRA_PIDS="${INFRA_PIDS} ${CFG_PID}"
    wait_health config-server http://127.0.0.1:8888/actuator/health

    EUREKA_PID="$(run_jvm discovery-server 384m "--server.port=8761" \
        "-Dspring.config.import=optional:configserver:http://127.0.0.1:8888")"
    INFRA_PIDS="${INFRA_PIDS} ${EUREKA_PID}"
    wait_health discovery-server http://127.0.0.1:8761/actuator/health

    ADMIN_PID="$(run_jvm admin-server 256m "--server.port=9090" \
        "-Dspring.config.import=optional:configserver:http://127.0.0.1:8888 -Deureka.client.serviceUrl.defaultZone=http://127.0.0.1:8761/eureka/")"
    INFRA_PIDS="${INFRA_PIDS} ${ADMIN_PID}"
    wait_health admin-server http://127.0.0.1:9090/actuator/health
fi

# ---- app tier: api-gateway + customers + vets + visits ---------------
APP_PIDS=""
if [ "$NANOVM_TIER" = "all" ] || [ "$NANOVM_TIER" = "app" ]; then
    # Under NANOVM_TIER=app the infra tier lives in another guest at
    # `nanovm-infra`; under `all` it's on 127.0.0.1. INFRA_HOST unifies
    # the two paths so the JVM args below stay identical.
    INFRA_HOST="127.0.0.1"
    if [ "$NANOVM_TIER" = "app" ]; then
        INFRA_HOST="nanovm-infra"
    fi
    APP_JVM_ARGS="-Dspring.config.import=optional:configserver:http://${INFRA_HOST}:8888 -Deureka.client.serviceUrl.defaultZone=http://${INFRA_HOST}:8761/eureka/"

    GATEWAY_PID="$(run_jvm api-gateway 256m "--server.port=8080" "$APP_JVM_ARGS")"
    APP_PIDS="${APP_PIDS} ${GATEWAY_PID}"

    CUSTOMERS_PID="$(run_jvm customers-service 256m "" "$APP_JVM_ARGS")"
    APP_PIDS="${APP_PIDS} ${CUSTOMERS_PID}"

    VETS_PID="$(run_jvm vets-service 256m "" "$APP_JVM_ARGS")"
    APP_PIDS="${APP_PIDS} ${VETS_PID}"

    VISITS_PID="$(run_jvm visits-service 256m "" "$APP_JVM_ARGS")"
    APP_PIDS="${APP_PIDS} ${VISITS_PID}"
fi

# All the PIDs, space-separated, in launch order. `warmup.sh` uses
# these to detect an early crash of any service via `kill -0`.
ALL_PIDS="${INFRA_PIDS} ${APP_PIDS}"

echo "[tier-launcher] launched pids:${ALL_PIDS}" >&2
export NANOVM_TIER ALL_PIDS
exec /warmup.sh
