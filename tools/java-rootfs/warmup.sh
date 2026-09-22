#!/bin/sh
# Guest-side warmup driver:
#
# 1. Wait for /actuator/health to return 200 (JVM booted, Spring
#    context fully loaded, embedded H2 initialised, tomcat listening).
# 2. Drive the request/JPA hot paths hard enough to push HotSpot past
#    C1 into C2. Duration-based (WARMUP_SECS, default 30 s) rather
#    than a small fixed pass count so the marker actually reflects a
#    warmed state.
# 3. Emit a single-line ready marker on the guest console so the host
#    can pattern-match it and take the warm snapshot. The pattern is
#    exactly `NANOVM_PETCLINIC_READY` on a line by itself — a follow-up
#    milestone will switch this to a vsock signal for lower latency,
#    but console-tail keeps the initial scaffold simple.
# 4. `wait` on the JVM so PID 1 (this script) exits when the JVM exits.
#    The kernel then panics — which is the intended failure mode: the
#    host sees the panic on stdio and knows the guest is unrecoverable,
#    no zombie.
#
# Usage: /warmup.sh <jvm-pid>

set -eu

JVM_PID="${1:-}"
HEALTH_URL="http://127.0.0.1:8080/actuator/health"
WARMUP_URLS="http://127.0.0.1:8080/vets \
             http://127.0.0.1:8080/vets.html \
             http://127.0.0.1:8080/owners \
             http://127.0.0.1:8080/owners/find"

READY_WAIT_SECS="${READY_WAIT_SECS:-60}"
READY_STEP=1
WARMUP_SECS="${WARMUP_SECS:-30}"

# ---- 1. wait for /actuator/health to be 200 --------------------------
echo "[warmup] waiting up to ${READY_WAIT_SECS}s for ${HEALTH_URL}" >&2
elapsed=0
status=000
while [ "$elapsed" -lt "$READY_WAIT_SECS" ]; do
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
    sleep "$READY_STEP"
    elapsed=$((elapsed + READY_STEP))
done
if [ "$status" != "200" ]; then
    echo "[warmup] FATAL: health never returned 200 within ${READY_WAIT_SECS}s" >&2
    cat /tmp/petclinic.log >&2 || true
    exit 1
fi

# ---- 2. duration-based JIT warmup ------------------------------------
# Every request goes through curl with `--fail`, so a 5xx from Petclinic
# fails the script rather than being masked. `hit_count` is reported at
# the end so the host log records how much work the warmup actually did
# (useful when tuning WARMUP_SECS).
echo "[warmup] jit warmup: ${WARMUP_SECS}s across ${WARMUP_URLS}" >&2
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
    echo "[warmup] FATAL: ${fail_count} warmup requests failed (5xx or transport)" >&2
    cat /tmp/petclinic.log >&2 || true
    exit 1
fi

# ---- 3. ready marker -------------------------------------------------
# The host's bench binary tails guest stdio for this exact string. Do
# NOT format-change without updating the host side.
echo "NANOVM_PETCLINIC_READY"

# ---- 4. wait on JVM so the guest kernel panics on JVM death ----------
# `wait <pid>` is safe for a child process even from PID 1: since this
# script `exec`ed here from /sbin/init, the JVM is our direct child.
# When the JVM exits (crash or clean shutdown), we exit; PID 1 exit
# → kernel panic on the guest → host sees it on stdio.
if [ -n "$JVM_PID" ]; then
    wait "$JVM_PID" || true
    echo "[warmup] JVM (pid=$JVM_PID) exited; init returning" >&2
fi
