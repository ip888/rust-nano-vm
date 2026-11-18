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
# 4. Poll the JVM PID so PID 1 (this script) exits when the JVM exits.
#    `init.sh` uses `exec` to become this script, so the new shell no
#    longer owns the job table entry required by `wait`; using `wait`
#    here returns 127 immediately and incorrectly kills a healthy VM.
#
# Usage: /warmup.sh <jvm-pid>

set -eu

JVM_PID="${1:-}"
HEALTH_URL="http://127.0.0.1:8080/actuator/health"
WARMUP_URLS="http://127.0.0.1:8080/vets \
             http://127.0.0.1:8080/vets.html \
             http://127.0.0.1:8080/owners \
             http://127.0.0.1:8080/owners/find"

READY_WAIT_SECS="${READY_WAIT_SECS:-120}"
READY_STEP=1
WARMUP_SECS="${WARMUP_SECS:-30}"
# On 1.6 GHz Broadwell with 1 vCPU (our reference host), Spring
# Petclinic cold-start takes 30-60 s just to bind :8080, before
# /actuator/health is 200. A 30 s guest-side wait tripped the
# FATAL `TCP socket never opened` branch on the first live run
# (serial tail cut at `[warmup] waiting up to 30s` because the
# host side killed the VM at --cold-marker-secs=30 before the
# guest even finished its loop). Match the generous host default
# that nanovm-jvm-bench's --cold-marker-secs now takes; Spring
# binding faster on quicker hosts simply makes the loop exit
# earlier.
COLD_WAIT_SECS="${COLD_WAIT_SECS:-180}"

# ---- 0. wait for the JVM to actually be running (TCP socket open) ----
#
# `init.sh` prints `[init] launching JVM` BEFORE fork+exec of `java`,
# so that line alone can't be used as the cold-snapshot synchronisation
# point — the guest may still be executing shell code or mid-execve
# when a host tailing the serial buffer sees it. We instead poll for
# the TCP socket the JVM binds (:8080). curl exits with code 7
# ("Failed to connect") until the socket is open, non-7 once it is,
# so the loop terminates the first time Petclinic's server socket
# accepts connections — a well-defined, kernel-observable state.
#
# The COLD marker is emitted after that check succeeds; the host
# reads it as "JVM is running, Spring init is in progress, no
# hand-off has happened yet". That's the state the cold snapshot
# is supposed to capture.
echo "[warmup] waiting up to ${COLD_WAIT_SECS}s for JVM TCP socket to open" >&2
elapsed=0
while [ "$elapsed" -lt "$COLD_WAIT_SECS" ]; do
    if [ -n "$JVM_PID" ] && ! kill -0 "$JVM_PID" 2>/dev/null; then
        echo "[warmup] FATAL: JVM (pid=$JVM_PID) exited before TCP open" >&2
        cat /tmp/petclinic.log >&2 || true
        exit 1
    fi
    # curl exit 7 = connection refused. Any other exit (including
    # success or protocol error) means the socket accepted.
    #
    # The `|| rc=$?` pattern is essential here: `set -e` at the top
    # of this script would otherwise kill the shell on curl's 7
    # before we could capture $? on the next line. The `||` branch
    # suppresses the set-e trigger AND captures the real exit code
    # into `rc` in one step. Without this guard the loop never iterates
    # and PID 1 exits 7 (the classic silent-warmup bug that masked all
    # of our dev-node triage: the real failure was always here, not
    # in the kernel-console routing).
    rc=0
    curl -sS -o /dev/null --max-time 1 http://127.0.0.1:8080/ 2>/dev/null || rc=$?
    if [ "$rc" != 7 ]; then
        echo "[warmup] JVM TCP socket open after ${elapsed}s" >&2
        break
    fi
    sleep 1
    elapsed=$((elapsed + 1))
done
if [ "$rc" = 7 ]; then
    echo "[warmup] FATAL: JVM TCP socket never opened within ${COLD_WAIT_SECS}s" >&2
    cat /tmp/petclinic.log >&2 || true
    exit 1
fi

# Ready-for-cold-snapshot marker. Host bench binary reads this.
echo "NANOVM_PETCLINIC_COLD"

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
printf '%s\n' ready > /run/nanovm-ready
echo "NANOVM_PETCLINIC_READY"

# ---- 4. supervise JVM so the guest kernel panics on JVM death --------
# After `exec`, the kernel still considers the JVM a child of PID 1,
# but the replacement shell has lost the original shell's job table.
# `wait "$JVM_PID"` therefore returns 127 ("not a child") even while
# the JVM is healthy. `kill -0` remains valid and gives us the intended
# supervision without relying on shell-local bookkeeping.
if [ -n "$JVM_PID" ]; then
    while kill -0 "$JVM_PID" 2>/dev/null; do
        if [ -r "/proc/$JVM_PID/stat" ]; then
            jvm_state="$(
                /bin/busybox awk '{ print $3 }' "/proc/$JVM_PID/stat" 2>/dev/null || true
            )"
            [ "$jvm_state" = "Z" ] && break
        else
            break
        fi
        sleep 1
    done
    echo "[warmup] JVM (pid=$JVM_PID) is no longer running; init returning" >&2
    echo "[warmup] JVM log tail:" >&2
    /bin/busybox tail -n 80 /tmp/petclinic.log >&2 || true
fi
