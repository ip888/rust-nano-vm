# Operator runbook

Audience: on-call engineers running `nanovm-control-plane` against
real workloads. Pre-1.0 — expect this to evolve milestone by
milestone. Cross-references `docs/threat-model.md` for the security
context behind each control here.

If a section starts with a triggering symptom (alert text, on-call
page, user report), it's a playbook. Otherwise it's reference.

## Service definition

| Property | Value |
| --- | --- |
| Binary | `nanovm-control-plane` |
| Default bind | `127.0.0.1:8080` |
| Healthcheck | `GET /healthz` → `200 ok` (no auth) |
| Metrics | `GET /metrics` (Prometheus 0.0.4, no auth) |
| OpenAPI spec | `GET /openapi.json` (no auth) |
| Logs | stdout, `tracing-subscriber` JSON when `RUST_LOG=info,nanovm_control_plane=info` and a json subscriber is wired |
| Container image | `Dockerfile` → distroless, uid 65532 |
| Shutdown signal | `SIGTERM` or `SIGINT` (Ctrl-C) — drains in-flight requests |

## Required environment

| Variable | Default | Notes |
| --- | --- | --- |
| `NANOVM_CONTROL_PLANE_ADDR` | `127.0.0.1:8080` | Bind to `0.0.0.0:8080` inside containers |
| `NANOVM_API_TOKENS` | *(unset)* | Comma-separated bearer tokens. **Empty disables auth** — operator MUST set this for any reachable deployment |
| `NANOVM_RATE_LIMIT_RPS` | `100` | Per-token bucket refill rate. `0` disables (WARN on startup) |
| `NANOVM_RATE_LIMIT_BURST` | `=rps` | Bucket capacity |
| `RUST_LOG` | `info` | `tracing` filter directive |

## Startup checklist

A correct boot logs three lines (or two WARN lines if you're running
auth/limiter disabled — fix that):

```
INFO  bearer-token auth enabled count=N
INFO  per-token rate limit enabled rps=100 burst=100
INFO  nanovm-control-plane listening addr=0.0.0.0:8080
```

If you see either of the following, you are **misconfigured**:

```
WARN  NANOVM_API_TOKENS is empty — /v1/* is unauthenticated.
WARN  NANOVM_RATE_LIMIT_RPS=0 — /v1/* is unthrottled.
```

These are intentional dev-mode escape hatches. They must not be
present in a deployment reachable from anywhere other than
`localhost`.

## Common operations

### Rotate API tokens

The binary reads `NANOVM_API_TOKENS` once at startup and holds the
set in memory. There is **no live-reload**. To rotate:

1. Issue new tokens to all clients.
2. Restart the binary with the **union** of old + new tokens in
   `NANOVM_API_TOKENS` for the grace window you want clients to
   migrate in.
3. Restart again with only the new tokens.

Use a process manager (systemd, Kubernetes Deployment) that waits
for `/healthz` to come up before promoting the new replica. The
`SIGTERM` handler drains in-flight requests, so a rolling restart
under a load balancer is loss-free for completed requests; long-
running streamed handlers (M4+) will be canceled — expect retries.

### Scrape `/metrics`

```sh
curl -s localhost:8080/metrics
```

Sample alert thresholds (Grafana / Alertmanager):

| Symptom | Expression | Severity |
| --- | --- | --- |
| Backend errors trending up | `rate(nanovm_http_requests_by_class_total{class="5xx"}[5m]) > 0.1` | warn |
| 4xx flood (client bug, attack, or token leak) | `rate(nanovm_http_requests_by_class_total{class="4xx"}[5m]) > 10` | warn |
| Saturation (inflight bumping the connection limit) | `nanovm_http_inflight > 500` | page |
| Liveness | `up{job="nanovm-control-plane"} == 0` | page |

`/metrics` exposes no per-route or per-token labels (deliberate —
those explode cardinality). For per-token usage, parse the access
log instead.

### Correlate a single request

Every response carries `X-Request-Id`. Callers can supply their own
(`[A-Za-z0-9._-]`, max 128 chars) and we'll echo it; otherwise we
mint one of shape `nanovm-{nanos}-{counter}`. Grep `tracing` output:

```sh
journalctl -u nanovm-control-plane --since "10 minutes ago" \
  | grep 'request_id="nanovm-019234-…"'
```

### Suspected token leak

Symptoms: anomalous `class="2xx"` or `class="429"` rate spike on the
metrics dashboard, unexpected source IPs in the access log.

1. **Rotate the suspected token immediately** (see above).
2. Pull the access log range covering the spike.
3. Cross-reference `X-Request-Id` across logs and downstream systems
   to see what the attacker touched.
4. File a security advisory if user data was exposed.

### Snapshot directory fills the disk

There is currently no built-in eviction. Operate the snapshot dir
behind one of:

- A timer that deletes manifests older than $RETENTION_DAYS.
- A quota on the underlying filesystem.

`DELETE /v1/snapshots/:id` is the API-side knob for explicit
cleanup.

## Triage playbooks

### "Service returning 401 for everything"

1. `curl localhost:8080/healthz` — if 200, the binary is up.
2. Check `NANOVM_API_TOKENS` is the value clients are presenting
   (env var, not file). Common pitfall: a stray newline at the end
   of the env var splits the token.
3. Confirm clients send `Authorization: Bearer <token>` (case-
   insensitive header name, `Bearer ` prefix required).

### "Service returning 429 for everything"

1. Check `/metrics` for `nanovm_http_inflight` — if it's stuck high
   the backend is wedged; restart.
2. Verify `NANOVM_RATE_LIMIT_RPS` matches the legitimate caller's
   actual rate. If you're sharing one token across many callers,
   raise the rate or shard tokens.
3. The 429 response carries `Retry-After` (integer seconds, ceiling)
   — clients should back off. If a client doesn't honour it, that's
   a client bug.

### "/v1 returns 500 with code=backend"

Backend failure surfaced from the `Hypervisor` trait. The structured
envelope's `message` field carries the human-readable detail.
Triage steps:

1. Grep logs for the matching `X-Request-Id`.
2. If the backend is `vm-mock`, this is a project bug — file an
   issue with the request, response, and stack trace.
3. If the backend is `vm-kvm`, check `/dev/kvm` permissions, host
   kernel version, and `dmesg | tail` for KVM errors.

### "POST /v1/vms succeeds but /v1/vms is empty"

The list endpoint enriches each entry with backend metadata
(`vm_meta`); entries that race a destroy or whose backend doesn't
support metadata fall through to an id-only row. If the list is
totally empty after a successful create:

1. Confirm both calls used the same `Authorization` token (a
   separate token + a separate process backend = separate state).
2. Check `nanovm_http_requests_total` — if it went up by 2, both
   requests reached the server.
3. If the backend is `vm-mock`, this is a project bug — file an
   issue.

### "Shutdown took longer than expected"

The graceful-drain path waits for in-flight requests to complete
before exiting. There is **no upper bound** on the drain budget
today (tracked as G3 in the threat model). If a handler hangs the
process won't exit — kill with SIGKILL. Follow-up PR A4 will add a
configurable drain timeout.

## Capacity guidance

Single-process limits (M6 control plane on `vm-mock`):

| Resource | Soft limit | Why |
| --- | --- | --- |
| Concurrent requests | ~10k | Per-listener tokio backlog; bind queue length is host-tunable |
| Per-token rate | `NANOVM_RATE_LIMIT_RPS` | Token-bucket, in-memory |
| VM count | host RAM ÷ guest RAM | `vm-mock` is in-memory; real backends are bounded by host |
| Snapshot count | inodes on snapshot dir | No eviction; see "Snapshot directory fills the disk" |

The bucket map for the rate limiter grows with the number of
**distinct valid bearer tokens** observed. There is no eviction
today — a malicious caller cannot grow it without first
authenticating (auth runs before rate-limit), so the bound is the
number of tokens you've issued. Follow-up: G10.

## Where to file things

- **Crash or wrong behaviour:** GitHub issue with `X-Request-Id`,
  logs around the failure, and the request body if any.
- **Security vulnerability:** GitHub Security Advisory (private).
  See `docs/threat-model.md` § Disclosure.
- **Performance regression:** issue with a `cargo run -p
  nanovm-bench` (M2+) before/after, plus host CPU/RAM specs.

## Change log

- **2026-05-16.** First draft. Reflects M0–M1 + M6 control-plane
  surfaces present on `main`: auth, rate limit, /metrics, request-id,
  Dockerfile. Update each time a new operator-visible knob lands.
