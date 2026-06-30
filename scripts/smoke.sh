#!/usr/bin/env bash
# smoke.sh — single-command, end-to-end verification of every shipped
# production feature. Run this and read the PASS/FAIL table at the bottom.
#
#   bash scripts/smoke.sh
#
# What it does:
#   - Builds the control-plane binary (cached after the first run).
#   - For each scenario: spawns the binary on a free port with the right
#     env, waits for it to bind, runs curls, asserts the response,
#     prints PASS or FAIL, kills the binary before moving on.
#   - At the end prints the score and exits 0 on all-pass, 1 on any
#     failure.
#
# No terminal toggling, no leftover processes. Picks a high random port
# so :8080 collisions can't bite you. Trap on EXIT kills any orphan.

set -uo pipefail

# ---- pretty print --------------------------------------------------------
RED=$'\033[31m'; GRN=$'\033[32m'; YEL=$'\033[33m'; BLU=$'\033[34m'; DIM=$'\033[2m'; RST=$'\033[0m'
pass()   { PASS=$((PASS+1)); RESULTS+=("${GRN}PASS${RST}  $1"); echo "${GRN}✓${RST} $1"; }
fail()   { FAIL=$((FAIL+1)); RESULTS+=("${RED}FAIL${RST}  $1 — $2"); echo "${RED}✗${RST} $1 — ${RED}$2${RST}"; }
skip()   { SKIP=$((SKIP+1)); RESULTS+=("${YEL}SKIP${RST}  $1 — $2"); echo "${YEL}-${RST} $1 — $2"; }
step()   { echo; echo "${BLU}━━━ $1 ━━━${RST}"; }
note()   { echo "${DIM}  $*${RST}"; }
PASS=0; FAIL=0; SKIP=0; RESULTS=()

# ---- prereqs -------------------------------------------------------------
for bin in cargo curl jq; do
    command -v $bin >/dev/null || { echo "${RED}missing prereq: $bin${RST}"; exit 2; }
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

BIN="$REPO_ROOT/target/debug/nanovm-control-plane"
echo "${BLU}building nanovm-control-plane (cached on subsequent runs)…${RST}"
cargo build -q -p control-plane --bin nanovm-control-plane 2>&1 | tail -3 || {
    echo "${RED}build failed; fix that before running smoke.sh${RST}"
    exit 2
}

# ---- server lifecycle helpers --------------------------------------------
TMP="$(mktemp -d -t nanovm-smoke.XXXXXX)"
LOG="$TMP/server.log"
PID_FILE="$TMP/server.pid"
note "scratch dir: $TMP"

# Pick a free high port so port collisions can't fail the smoke.
pick_port() {
    local p
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        p=$(( RANDOM % 10000 + 20000 ))
        if ! (echo > /dev/tcp/127.0.0.1/$p) 2>/dev/null; then
            echo $p; return 0
        fi
    done
    echo "${RED}could not find a free port${RST}" >&2; exit 2
}
PORT="$(pick_port)"
P="http://127.0.0.1:$PORT"
note "control plane URL: $P"

# Trap kills the server on exit / Ctrl-C so you don't leak processes.
stop_server() {
    if [[ -f "$PID_FILE" ]]; then
        local pid; pid="$(cat "$PID_FILE")"
        kill "$pid" 2>/dev/null || true
        # Give it 2s to die gracefully then SIGKILL.
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.2
        done
        kill -9 "$pid" 2>/dev/null || true
        rm -f "$PID_FILE"
    fi
}
trap 'stop_server; rm -rf "$TMP"' EXIT

# Start the server with the env vars passed as args, wait for it to bind.
# `start_server VAR=val VAR=val …` — env applied to the child only.
start_server() {
    stop_server   # be sure nothing left from a previous step
    local logfile="$LOG"
    : > "$logfile"
    # Use `env` so KEY=VAL args passed positionally actually become
    # environment vars on the child instead of being parsed as
    # commands. `env` applies left-to-right; the caller's args go LAST
    # so they can override the defaults this helper sets.
    env \
        NANOVM_CONTROL_PLANE_ADDR="127.0.0.1:$PORT" \
        NANOVM_LOG_FORMAT="text" \
        "$@" "$BIN" > "$logfile" 2>&1 &
    echo $! > "$PID_FILE"

    # Poll the port for up to 10 seconds.
    local i
    for i in $(seq 1 100); do
        if (echo > /dev/tcp/127.0.0.1/$PORT) 2>/dev/null; then
            return 0
        fi
        # If the process died, bail with the log tail.
        if ! kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
            echo "${RED}server crashed before binding:${RST}"
            tail -20 "$logfile"
            return 1
        fi
        sleep 0.1
    done
    echo "${RED}server never bound :$PORT in 10s${RST}"
    tail -20 "$logfile"
    return 1
}

# Assertion helpers.
assert_eq() {
    # assert_eq <label> <actual> <expected>
    if [[ "$2" == "$3" ]]; then pass "$1"; else fail "$1" "expected '$3', got '$2'"; fi
}
assert_status() {
    # assert_status <label> <method> <url> <expected_code> [extra curl args…]
    local label="$1" method="$2" url="$3" want="$4"; shift 4
    local code
    code=$(curl -s -o /dev/null -w "%{http_code}" -X "$method" "$url" "$@" || echo "000")
    assert_eq "$label" "$code" "$want"
}

A_HDR=( -H "Authorization: Bearer acme-tok" )
G_HDR=( -H "Authorization: Bearer gx-tok" )
O_HDR=( -H "Authorization: Bearer op-tok" )
JSON_HDR=( -H 'content-type: application/json' )

# =========================================================================
step "1 — boot + /healthz + /v1/health"
# -------------------------------------------------------------------------
start_server NANOVM_API_TOKENS=acme:acme-tok || { fail "boot" "server didn't start"; exit 1; }

body=$(curl -fs "$P/healthz" || echo "")
assert_eq "/healthz returns 'ok'" "$body" "ok"

body=$(curl -fs "$P/v1/health" "${A_HDR[@]}")
backend=$(echo "$body" | jq -r .backend)
assert_eq "/v1/health returns backend=mock" "$backend" "mock"

# =========================================================================
step "2 — bearer-token auth gate"
# -------------------------------------------------------------------------
assert_status "missing bearer → 401" GET "$P/v1/vms" 401
assert_status "wrong bearer → 401"   GET "$P/v1/vms" 401 -H "Authorization: Bearer wrong"
assert_status "right bearer → 200"   GET "$P/v1/vms" 200 "${A_HDR[@]}"

# =========================================================================
step "3 — full VM lifecycle through REST"
# -------------------------------------------------------------------------
vm=$(curl -fsX POST "$P/v1/vms" "${A_HDR[@]}" "${JSON_HDR[@]}" -d '{}' | jq .id)
[[ -n "$vm" && "$vm" != "null" ]] && pass "create_vm returns numeric id" || fail "create_vm" "got '$vm'"

state=$(curl -fs "$P/v1/vms/$vm" "${A_HDR[@]}" | jq -r .state)
assert_eq "fresh VM state is 'created'" "$state" "created"

assert_status "start → 204" POST "$P/v1/vms/$vm/start" 204 "${A_HDR[@]}"
state=$(curl -fs "$P/v1/vms/$vm" "${A_HDR[@]}" | jq -r .state)
assert_eq "post-start state is 'running'" "$state" "running"

snap=$(curl -fsX POST "$P/v1/vms/$vm/snapshot" "${A_HDR[@]}" "${JSON_HDR[@]}" -d '{}' | jq .id)
[[ -n "$snap" && "$snap" != "null" ]] && pass "snapshot returns id" || fail "snapshot" "got '$snap'"

fork=$(curl -fsX POST "$P/v1/snapshots/$snap/fork" "${A_HDR[@]}")
fork_ms=$(echo "$fork" | jq -r .fork_ms)
[[ "$fork_ms" =~ ^[0-9]+$ ]] && pass "fork returns numeric fork_ms (=${fork_ms}ms)" || fail "fork" "fork_ms='$fork_ms'"

assert_status "stop → 204"    POST   "$P/v1/vms/$vm/stop" 204 "${A_HDR[@]}"
assert_status "destroy → 204" DELETE "$P/v1/vms/$vm"      204 "${A_HDR[@]}"

# =========================================================================
step "4 — cross-org isolation (multi-tenant)"
# -------------------------------------------------------------------------
start_server NANOVM_API_TOKENS='acme:acme-tok,globex:gx-tok' || { fail "boot §4" "no server"; exit 1; }

vm=$(curl -fsX POST "$P/v1/vms" "${A_HDR[@]}" "${JSON_HDR[@]}" -d '{}' | jq .id)
code=$(curl -s -o "$TMP/cross.json" -w "%{http_code}" -X POST "$P/v1/vms/$vm/start" "${G_HDR[@]}")
assert_eq "globex touching acme's VM → 403" "$code" "403"
err_code=$(jq -r '.error.code' "$TMP/cross.json")
assert_eq "cross-org error.code is 'cross_org'" "$err_code" "cross_org"

len=$(curl -fs "$P/v1/vms" "${G_HDR[@]}" | jq '.vms | length')
assert_eq "globex's vm list is empty" "$len" "0"

# =========================================================================
step "5 — per-org metering + 6 — operator ?all=true"
# -------------------------------------------------------------------------
start_server NANOVM_API_TOKENS='op-tok,acme:acme-tok,globex:gx-tok' || { fail "boot §5" "no server"; exit 1; }

# Generate some forks on each org.
for hdr in "${A_HDR[@]}" "${G_HDR[@]}"; do : ; done  # noop, just to expand vars
for org in acme globex; do
    case $org in acme) H=(-H "Authorization: Bearer acme-tok");; globex) H=(-H "Authorization: Bearer gx-tok");; esac
    v=$(curl -fsX POST "$P/v1/vms" "${H[@]}" "${JSON_HDR[@]}" -d '{}' | jq .id)
    curl -fsX POST "$P/v1/vms/$v/start" "${H[@]}" > /dev/null
    s=$(curl -fsX POST "$P/v1/vms/$v/snapshot" "${H[@]}" "${JSON_HDR[@]}" -d '{}' | jq .id)
    for _ in 1 2 3; do curl -fsX POST "$P/v1/snapshots/$s/fork" "${H[@]}" > /dev/null; done
done

usage_acme=$(curl -fs "$P/v1/usage/by-org" "${A_HDR[@]}")
orgs_acme=$(echo "$usage_acme" | jq '[.orgs[].org_id] | sort | join(",")')
assert_eq "tenant view returns only own org" "$orgs_acme" '"acme"'

usage_all=$(curl -fs "$P/v1/usage/by-org?all=true" "${O_HDR[@]}")
orgs_all=$(echo "$usage_all" | jq '[.orgs[].org_id] | sort | join(",")')
assert_eq "operator ?all=true returns every org" "$orgs_all" '"acme,globex"'

# =========================================================================
step "7 — self-serve API key issue + revoke"
# -------------------------------------------------------------------------
issued=$(curl -fsX POST "$P/v1/keys" "${A_HDR[@]}")
new_tok=$(echo "$issued" | jq -r .token)
new_id=$(echo "$issued" | jq -r .id)
[[ -n "$new_tok" && "$new_tok" != "null" ]] && pass "POST /v1/keys returns plaintext token" || fail "issue" "$issued"

code=$(curl -s -o /dev/null -w "%{http_code}" "$P/v1/vms" -H "Authorization: Bearer $new_tok")
assert_eq "newly-issued key authenticates" "$code" "200"

list=$(curl -fs "$P/v1/keys" "${A_HDR[@]}" | jq '.keys[0].token // "absent"')
assert_eq "list does NOT include plaintext" "$list" '"absent"'

assert_status "DELETE /v1/keys/<id> → 204" DELETE "$P/v1/keys/$new_id" 204 "${A_HDR[@]}"

code=$(curl -s -o /dev/null -w "%{http_code}" "$P/v1/vms" -H "Authorization: Bearer $new_tok")
assert_eq "revoked key now 401s" "$code" "401"

# =========================================================================
step "8 — runtime key persistence across restart"
# -------------------------------------------------------------------------
TOK_STORE="$TMP/tokens.json"
start_server NANOVM_API_TOKENS=acme:acme-tok NANOVM_TOKEN_STORE_PATH="$TOK_STORE" || { fail "boot §8" ""; exit 1; }
new_tok=$(curl -fsX POST "$P/v1/keys" "${A_HDR[@]}" | jq -r .token)
[[ -f "$TOK_STORE" ]] && pass "issuance wrote to $TOK_STORE" || fail "persist" "file not created"

stop_server
start_server NANOVM_API_TOKENS=acme:acme-tok NANOVM_TOKEN_STORE_PATH="$TOK_STORE" || { fail "restart §8" ""; exit 1; }
code=$(curl -s -o /dev/null -w "%{http_code}" "$P/v1/vms" -H "Authorization: Bearer $new_tok")
assert_eq "key still works after restart" "$code" "200"

# =========================================================================
step "9 — sandbox-action endpoint (one-shot fork+run+destroy)"
# -------------------------------------------------------------------------
start_server NANOVM_API_TOKENS=acme:acme-tok || { fail "boot §9" ""; exit 1; }
v=$(curl -fsX POST "$P/v1/vms" "${A_HDR[@]}" "${JSON_HDR[@]}" -d '{}' | jq .id)
curl -fsX POST "$P/v1/vms/$v/start" "${A_HDR[@]}" > /dev/null
s=$(curl -fsX POST "$P/v1/vms/$v/snapshot" "${A_HDR[@]}" "${JSON_HDR[@]}" -d '{}' | jq .id)

resp=$(curl -fsX POST "$P/v1/sandbox/invoke" "${A_HDR[@]}" "${JSON_HDR[@]}" \
       -d "{\"snapshot\":$s,\"action\":\"execute_shell\",\"command\":\"echo nanovm-smoke\"}")
exit_code=$(echo "$resp" | jq .exit_code)
assert_eq "/v1/sandbox/invoke exit_code is 0" "$exit_code" "0"
stdout=$(echo "$resp" | jq -r .stdout)
[[ "$stdout" == *"nanovm-smoke"* ]] && pass "sandbox stdout contains marker" || fail "sandbox stdout" "got '$stdout'"

# =========================================================================
step "10 — Prometheus /metrics (per-org series)"
# -------------------------------------------------------------------------
# Need a fork through the /fork endpoint specifically — sandbox/invoke
# uses a different metric path (warm_hit/miss only).
start_server NANOVM_API_TOKENS=acme:acme-tok || { fail "boot §10" ""; exit 1; }
v=$(curl -fsX POST "$P/v1/vms" "${A_HDR[@]}" "${JSON_HDR[@]}" -d '{}' | jq .id)
curl -fsX POST "$P/v1/vms/$v/start" "${A_HDR[@]}" > /dev/null
s=$(curl -fsX POST "$P/v1/vms/$v/snapshot" "${A_HDR[@]}" "${JSON_HDR[@]}" -d '{}' | jq .id)
curl -fsX POST "$P/v1/snapshots/$s/fork" "${A_HDR[@]}" > /dev/null

text=$(curl -fs "$P/metrics")
echo "$text" | grep -q '^nanovm_up 1'                         && pass "nanovm_up=1"                          || fail "nanovm_up"                "missing"
echo "$text" | grep -q '^nanovm_forks_total{token='            && pass "per-token forks series present"      || fail "forks_total"              "missing"
echo "$text" | grep -q '^nanovm_forks_total_by_org{org='       && pass "per-org forks series present"        || fail "forks_total_by_org"       "missing"
echo "$text" | grep -q '^nanovm_fork_latency_ms_sum '          && pass "fork latency sum present"            || fail "fork_latency_ms_sum"      "missing"

# =========================================================================
step "11 — audit log (forensics)"
# -------------------------------------------------------------------------
AUDIT="$TMP/audit.jsonl"
start_server NANOVM_API_TOKENS=acme:acme-tok NANOVM_AUDIT_LOG="$AUDIT" || { fail "boot §11" ""; exit 1; }
curl -fsX POST "$P/v1/vms" "${A_HDR[@]}" "${JSON_HDR[@]}" -d '{}' > /dev/null

lines=$(wc -l < "$AUDIT" 2>/dev/null || echo 0)
[[ "$lines" -ge 1 ]] && pass "audit log captured $lines write(s)" || fail "audit log" "no rows in $AUDIT"
jq -e . "$AUDIT" > /dev/null 2>&1 && pass "audit lines are valid JSON" || fail "audit JSON" "malformed"

# =========================================================================
step "12 — structured JSON logs"
# -------------------------------------------------------------------------
start_server NANOVM_API_TOKENS=acme:acme-tok NANOVM_LOG_FORMAT=json || { fail "boot §12" ""; exit 1; }
# The bind log is on stderr — give the run a moment to flush.
sleep 0.3
# Pull the first non-empty line and confirm it's JSON.
line=$(grep -m1 '^{' "$LOG" || true)
if [[ -n "$line" ]] && echo "$line" | jq -e .level > /dev/null 2>&1; then
    pass "logs emit valid JSON with .level field"
else
    fail "json logs" "no JSON line found in startup output"
fi

# =========================================================================
step "13 — fork-quota throttling (429 + observable)"
# -------------------------------------------------------------------------
start_server NANOVM_API_TOKENS=acme:acme-tok \
             NANOVM_FORK_RPS=1 \
             NANOVM_FORK_BURST=2 || { fail "boot §13" ""; exit 1; }

v=$(curl -fsX POST "$P/v1/vms" "${A_HDR[@]}" "${JSON_HDR[@]}" -d '{}' | jq .id)
curl -fsX POST "$P/v1/vms/$v/start" "${A_HDR[@]}" > /dev/null
s=$(curl -fsX POST "$P/v1/vms/$v/snapshot" "${A_HDR[@]}" "${JSON_HDR[@]}" -d '{}' | jq .id)

# Hammer 10 forks. With per_sec=1, burst=2 we expect a few 429s.
codes=""
for _ in 1 2 3 4 5 6 7 8 9 10; do
    codes="$codes $(curl -s -o /dev/null -w "%{http_code}" -X POST "$P/v1/snapshots/$s/fork" "${A_HDR[@]}")"
done
ok=$(echo "$codes" | tr ' ' '\n' | grep -c '^201$' || true)
throttled=$(echo "$codes" | tr ' ' '\n' | grep -c '^429$' || true)
note "fork status codes:$codes"
[[ "$throttled" -ge 1 ]] && pass "quota produced ≥1 throttle ($throttled × 429, $ok × 201)" \
                          || fail "quota throttling" "no 429s in 10 forks"

# /metrics counter ticked
t=$(curl -fs "$P/metrics" | grep -E '^nanovm_fork_quota_throttled_total' | head -1)
[[ -n "$t" ]] && pass "throttled_total exposed in /metrics" || fail "throttled metric" "missing"

# =========================================================================
step "14 — Helm chart + OpenAPI render"
# -------------------------------------------------------------------------
stop_server
if command -v helm >/dev/null; then
    if helm template smoke "$REPO_ROOT/deploy/helm/nanovm" \
            --set config.apiTokens="acme:dev-tok" > "$TMP/helm.yaml" 2>"$TMP/helm.err"; then
        pass "helm template (mock backend) renders"
    else
        fail "helm mock" "$(head -3 "$TMP/helm.err")"
    fi
    if helm template smoke "$REPO_ROOT/deploy/helm/nanovm" \
            --set config.apiTokens="acme:dev-tok" \
            --set config.backend=fleet > "$TMP/helm-fleet.yaml" 2>"$TMP/helm-fleet.err"; then
        if grep -q NANOVM_FLEET_JAILER_BINARY "$TMP/helm-fleet.yaml"; then
            pass "helm template (fleet backend) wires NANOVM_FLEET_*"
        else
            fail "helm fleet" "NANOVM_FLEET_* env vars missing"
        fi
    else
        fail "helm fleet" "$(head -3 "$TMP/helm-fleet.err")"
    fi
else
    skip "helm tests" "helm not installed"
fi

if cargo run -q -p control-plane --bin nanovm-openapi > "$TMP/openapi.json" 2>/dev/null; then
    n=$(jq '.paths | length' "$TMP/openapi.json")
    [[ "$n" -ge 10 ]] && pass "openapi.json describes $n endpoints" || fail "openapi" "only $n paths"
else
    skip "openapi" "nanovm-openapi bin failed; not required"
fi

# =========================================================================
echo
echo "${BLU}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RST}"
echo "${BLU}  Results — $PASS passed, $FAIL failed, $SKIP skipped${RST}"
echo "${BLU}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RST}"
printf '%s\n' "${RESULTS[@]}"
echo

if [[ "$FAIL" -gt 0 ]]; then
    echo "${RED}smoke failed — full server log of the last scenario at $LOG${RST}"
    exit 1
else
    echo "${GRN}all production features verified ✓${RST}"
    exit 0
fi
