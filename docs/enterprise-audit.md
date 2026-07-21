# Enterprise audit trail

The control plane ships two independent, best-effort audit outputs:

1. **Local JSONL file** (`NANOVM_AUDIT_LOG`, default build) — one line per
   mutating `/v1/*` call, appended to a filesystem path the operator
   controls. Rotation via `logrotate` with `copytruncate`.
2. **SIEM HTTP webhook sink** (`NANOVM_AUDIT_SINK_URL`, `--features audit-sink` build)
   — the same JSON record POSTed to a customer's Datadog / Splunk HEC /
   generic collector. Records that fail to POST are logged and dropped;
   a slow sink never stalls request threads.

Either or both may be configured. The record shape is identical across
both destinations.

## Record shape

```json
{
  "ts": "2026-07-21T14:03:12.418Z",
  "method": "POST",
  "path": "/v1/marketplace/snapshots/python-3.12-ds/fork",
  "status": 201,
  "token": "tok-abcd-42",
  "request_id": "0a1b2c3d4e5f"
}
```

- `token` is the non-cryptographic fingerprint `tok-<first4>-<len>`.
  The raw bearer never leaves the request.
- `request_id` is present when the request-id middleware ran (default
  in the shipped router).
- Only `POST` / `PUT` / `PATCH` / `DELETE` are recorded — the audit
  value is "who changed what", not "who looked".

## Configuring the file appender

```bash
export NANOVM_AUDIT_LOG=/var/lib/nanovm/audit.jsonl
nanovm-control-plane
```

Unset or empty → disabled (no file, no warning noise). Unwritable path
→ startup logs an `ERROR` once and continues with the appender
disabled (the binary boots so a log-config typo can't take down the
service).

## Configuring the SIEM sink

Requires a build with `--features audit-sink` (adds `reqwest` as a
dep). The 15 MB binary size cost buys enterprise-grade audit shipping
with no extra sidecars.

```bash
# Datadog HTTP Intake (any region).
export NANOVM_AUDIT_SINK_URL="https://http-intake.logs.datadoghq.com/api/v2/logs"
export NANOVM_AUDIT_SINK_HEADER="DD-API-KEY: <your-datadog-api-key>"

# ... or Splunk HEC:
export NANOVM_AUDIT_SINK_URL="https://splunk.example.com:8088/services/collector/event"
export NANOVM_AUDIT_SINK_HEADER="Authorization: Splunk <hec-token>"

# ... or a generic collector.
export NANOVM_AUDIT_SINK_URL="https://collector.example.com/nanovm-audit"
export NANOVM_AUDIT_SINK_HEADER="Authorization: Bearer <shared-secret>"

nanovm-control-plane
```

Only ONE extra header is supported today; add more via a reverse-proxy
if a collector needs a multi-header handshake (rare).

## Guarantees & non-guarantees

- **Guarantee**: every request the audit middleware sees produces one
  best-effort attempt to append to each configured sink. There is no
  "at-least-once" semantic — SIEM ingestion is observability, not the
  source of truth.
- **Guarantee**: neither sink ever stalls a request. File writes are
  synchronous under a mutex (microsecond scale); sink pushes are
  `try_send` into a bounded channel (nanoseconds, drop-on-full).
- **Not guaranteed**: ordering across the two sinks. A record may
  appear in the file before or after it appears at the SIEM
  collector.
- **Not guaranteed**: durability across process crash. Records
  in-flight in the sink channel are lost on `kill -9`. The file
  appender uses libc-buffered `write`, so up to one line may be lost
  on a crash between `write()` and the OS flush.
- **Not guaranteed**: retry on sink failure. A single POST attempt per
  record, no back-off. If the collector is down, the operator sees
  `tracing::warn!` lines and the record is dropped. The next
  successful POST resumes normal shipping.

## Prometheus signals

- `nanovm_audit_sink_channel_full` — WARN-level tracing counter (not
  yet exposed to Prometheus); a follow-up may promote it. For now the
  operator's log-aggregator answers "is the sink getting behind?"

## Compliance posture

- **SOC 2 CC7.2** (system operations — monitoring / logging): the
  JSONL file + SIEM sink together satisfy the "capture and retain
  security-relevant events" control.
- **HIPAA 164.312(b)** (audit controls): the record schema captures
  method + path + status + fingerprint + request id. Enough to
  reconstruct "who changed what" without capturing PHI in the URL.
- **ISO 27001 A.12.4** (event logging + protection): the SIEM sink
  externalizes audit to a system with independent write access; the
  local file survives sink outages.
