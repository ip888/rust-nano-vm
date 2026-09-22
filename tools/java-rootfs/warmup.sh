#!/bin/sh
# Guest-side warmup driver:
#
# 1. Wait for /actuator/health to return 200 (JVM booted, Spring
#    context fully loaded, embedded H2 initialised, tomcat listening).
# 2. Hit a few realistic endpoints to drive JIT past C1 into C2 on
#    the hot paths a benchmark actually cares about.
# 3. Emit a single-line ready marker on the guest console so the host
#    can pattern-match it and take the warm snapshot. The pattern is
#    exactly `NANOVM_PETCLINIC_READY` on a line by itself — a follow-up
#    commit will switch this to a vsock signal for lower latency, but
#    console-tail keeps the initial scaffold simple.
# 4. Sleep forever so the JVM keeps running and the host can snapshot
#    at any point.
#
# Usage: /warmup.sh <jvm-pid>

set -eu

JVM_PID="${1:-}"
HEALTH_URL="http://127.0.0.1:8080/actuator/health"
WARMUP_URLS="http://127.0.0.1:8080/vets \
             http://127.0.0.1:8080/vets.html \
             http://127.0.0.1:8080/owners \
             http://127.0.0.1:8080/owners/find"

WAIT_SECS=60
WAIT_STEP=1

echo "[warmup] waiting up to ${WAIT_SECS}s for ${HEALTH_URL}" >&2
elapsed=0
while [ "$elapsed" -lt "$WAIT_SECS" ]; do
    if [ -n "$JVM_PID" ] && ! kill -0 "$JVM_PID" 2>/dev/null; then
        echo "[warmup] FATAL: JVM (pid=$JVM_PID) exited before ready" >&2
        cat /tmp/petclinic.log >&2 || true
        exit 1
    fi
    status="$(curl -sS -o /dev/null -w '%{http_code}' \
                   --max-time 3 "$HEALTH_URL" 2>/dev/null || echo 000)"
    if [ "$status" = "200" ]; then
        echo "[warmup] health 200 after ${elapsed}s" >&2
        break
    fi
    sleep "$WAIT_STEP"
    elapsed=$((elapsed + WAIT_STEP))
done
if [ "$status" != "200" ]; then
    echo "[warmup] FATAL: health never returned 200 within ${WAIT_SECS}s" >&2
    cat /tmp/petclinic.log >&2 || true
    exit 1
fi

# JIT warmup: three passes over the target endpoints. Enough to push
# past C1 into C2 for /vets and /owners handlers plus JPA queries.
echo "[warmup] jit warmup: 3 passes across the demo endpoints" >&2
for pass in 1 2 3; do
    for url in $WARMUP_URLS; do
        curl -sS -o /dev/null --max-time 5 "$url" || true
    done
    echo "[warmup] pass ${pass} done" >&2
done

# Ready marker. The host's bench binary tails guest stdio for this
# exact string. Do NOT format-change without updating the host side.
echo "NANOVM_PETCLINIC_READY"

# Keep init alive so the JVM keeps running and remains snapshottable.
# `wait` on the JVM would exit as soon as the JVM does; a plain sleep
# loop makes the intent obvious.
while true; do sleep 3600; done
