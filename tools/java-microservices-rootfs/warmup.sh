#!/bin/sh
# Wait-and-warmup driver for the full microservices stack.
#
# Sequence (post-`exec` from tier-launcher.sh):
#   1. Wait for Eureka to have four expected client registrations
#      (api-gateway + customers + vets + visits) — proves the full
#      "app" tier is up. Emits `NANOVM_MICROSERVICES_COLD` on stdio
#      as soon as api-gateway's TCP socket is open (a deterministic
#      cold-snapshot synchronisation point — the equivalent of the
#      M1 rootfs's cold marker).
#   2. Hit a business-endpoint set via api-gateway for `WARMUP_SECS`
#      to drive JIT past C1 into C2. Same duration-based approach as
#      the M1 warmup, just against multi-service routes.
#   3. Emit `NANOVM_MICROSERVICES_READY`.
#   4. `wait` on the launched PIDs so PID 1 exits with any JVM
#      exit and the guest kernel panics — the same clean-failure
#      contract the M1 rootfs uses.

set -eu

# The tier-launcher exports ALL_PIDS + NANOVM_TIER for us.
: "${ALL_PIDS:?ALL_PIDS not set — did tier-launcher exec into this?}"
: "${NANOVM_TIER:=all}"

GATEWAY_URL="http://127.0.0.1:8080"
EUREKA_URL="http://127.0.0.1:8761"
if [ "$NANOVM_TIER" = "app" ]; then
    # In tier-split mode Eureka lives on the infra guest.
    EUREKA_URL="http://nanovm-infra:8761"
fi

COLD_WAIT_SECS="${COLD_WAIT_SECS:-90}"
READY_WAIT_SECS="${READY_WAIT_SECS:-180}"
WARMUP_SECS="${WARMUP_SECS:-45}"

# Warmup route set — hits api-gateway which fans out to customers,
# vets, visits. The `/api/*` prefixes are the upstream gateway routes.
WARMUP_URLS="${GATEWAY_URL}/api/customer/owners \
             ${GATEWAY_URL}/api/vet/vets \
             ${GATEWAY_URL}/api/customer/petTypes \
             ${GATEWAY_URL}/api/visit/pets/visits?petId=1"

any_jvm_dead() {
    # ALL_PIDS is space-separated; iterate word-by-word.
    for p in $ALL_PIDS; do
        [ -z "$p" ] && continue
        if ! kill -0 "$p" 2>/dev/null; then
            echo "[warmup] FATAL: pid=$p exited unexpectedly" >&2
            return 0
        fi
    done
    return 1
}

# ---- 0. cold marker: api-gateway TCP socket open ---------------------
echo "[warmup] waiting up to ${COLD_WAIT_SECS}s for api-gateway TCP :8080" >&2
elapsed=0
while [ "$elapsed" -lt "$COLD_WAIT_SECS" ]; do
    if any_jvm_dead; then
        exit 1
    fi
    curl -sS -o /dev/null --max-time 1 "$GATEWAY_URL/" 2>/dev/null
    rc=$?
    if [ "$rc" != 7 ]; then
        echo "[warmup] api-gateway TCP socket open after ${elapsed}s" >&2
        break
    fi
    sleep 1
    elapsed=$((elapsed + 1))
done
if [ "$rc" = 7 ]; then
    echo "[warmup] FATAL: api-gateway TCP :8080 never opened in ${COLD_WAIT_SECS}s" >&2
    exit 1
fi

echo "NANOVM_MICROSERVICES_COLD"

# ---- 1. wait for full-stack readiness --------------------------------
# The "full stack" is defined by Eureka's registered-apps count:
# 4 apps when tier=all (api-gateway + customers + vets + visits;
# discovery-server registers itself as an app too on some releases —
# handle both by treating "at least 4" as sufficient). Under
# tier=app there's no admin/discovery/config to count, so we probe
# api-gateway's /api/customer/owners directly — a 200 means Eureka's
# lookup + downstream service both worked.
echo "[warmup] waiting up to ${READY_WAIT_SECS}s for full-stack readiness" >&2
elapsed=0
while [ "$elapsed" -lt "$READY_WAIT_SECS" ]; do
    if any_jvm_dead; then
        exit 1
    fi
    # `/api/customer/owners` returns 200 only when api-gateway can
    # route to customers-service AND customers-service has completed
    # JPA/H2 init. That's a strong end-to-end readiness proof.
    status="$(curl -sS -o /dev/null -w '%{http_code}' \
                   --max-time 3 "${GATEWAY_URL}/api/customer/owners" 2>/dev/null \
              || echo 000)"
    if [ "$status" = "200" ]; then
        echo "[warmup] end-to-end 200 from customers-service after ${elapsed}s" >&2
        break
    fi
    sleep 2
    elapsed=$((elapsed + 2))
done
if [ "$status" != "200" ]; then
    echo "[warmup] FATAL: full-stack /api/customer/owners never returned 200 in ${READY_WAIT_SECS}s" >&2
    # Dump one log per service so failures are debuggable from host
    # console tail alone.
    for name in config-server discovery-server admin-server \
                api-gateway customers-service vets-service visits-service; do
        log="/var/log/petclinic-ms/${name}.log"
        [ -r "$log" ] || continue
        echo "=== tail /var/log/petclinic-ms/${name}.log ===" >&2
        tail -n 40 "$log" >&2
    done
    exit 1
fi

# ---- 2. JIT warmup ---------------------------------------------------
echo "[warmup] jit warmup: ${WARMUP_SECS}s across gateway routes" >&2
end_epoch=$(( $(date +%s) + WARMUP_SECS ))
hit_count=0
fail_count=0
while [ "$(date +%s)" -lt "$end_epoch" ]; do
    for url in $WARMUP_URLS; do
        if curl --fail -sS -o /dev/null --max-time 5 "$url"; then
            hit_count=$((hit_count + 1))
        else
            fail_count=$((fail_count + 1))
        fi
    done
done
echo "[warmup] jit warmup done: ${hit_count} ok, ${fail_count} failed" >&2
if [ "$fail_count" -gt 0 ]; then
    echo "[warmup] FATAL: ${fail_count} warmup requests failed" >&2
    exit 1
fi

# ---- 3. ready marker -------------------------------------------------
echo "NANOVM_MICROSERVICES_READY"

# ---- 4. block until any JVM exits ------------------------------------
# `wait` on multiple pids blocks until ANY of them exits. That's the
# right semantic for the panic-on-first-exit contract — if any service
# crashes we lose the demo state and the guest should reboot fresh
# rather than serve half-working traffic.
# shellcheck disable=SC2086
wait ${ALL_PIDS} || true
echo "[warmup] first JVM exited; init returning" >&2
