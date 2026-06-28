# Onboarding a new tenant

You signed a customer. They're paying for `acme:*` and want to start firing forks at your control plane. This doc is the playbook the operator runs to take them from "contract signed" to "billing data flowing" in ~10 minutes.

Prereqs:
- A running `nanovm-control-plane` reachable at `https://<your-endpoint>` (see [README.md](README.md) for deploy paths).
- The deployment was started with `NANOVM_API_TOKENS=` set (auth on). Required for production.

---

## Step 1 — Mint the tenant's bootstrap token (operator action)

Two shapes. Pick one.

### 1a. Bootstrap from env (single-host / first tenant on a deployment)

Edit your deployment's secret store and append the tenant to `NANOVM_API_TOKENS`:

```
NANOVM_API_TOKENS=acme:tok_$(openssl rand -hex 16),existing:tokens...
```

- Helm: `helm upgrade ... --set tokensSecret.existingSecret=...` or `--set config.apiTokens=...`
- Fly.io: `flyctl secrets set NANOVM_API_TOKENS="..."`
- bare-metal Docker: re-launch with the new `-e NANOVM_API_TOKENS=...`

The control plane is **stateful** on this env shape (parses once at startup), so this requires a redeploy. For self-serve key issuance without a redeploy, use 1b.

### 1b. Self-serve via `POST /v1/keys` (preferred for tenant N+1)

Once the tenant exists in the env (a single one-time `org:bootstrap-tok` is enough), hand them their bootstrap token and have them issue working keys themselves:

```sh
curl -s https://<your-endpoint>/v1/keys \
  -X POST \
  -H "Authorization: Bearer <bootstrap-token>"
```

Response:
```json
{
  "token": "nv_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
  "id":    "nvk_BBBBBBBBBBBBBBBBBBBBBB",
  "org":   "acme",
  "created_at": "2026-06-28T13:00:00.000Z"
}
```

The `token` is shown **once** — the tenant persists it client-side. Subsequent `GET /v1/keys` returns the `id` + `created_at` (no plaintext) so they can rotate via `DELETE /v1/keys/{id}` later.

Note: runtime-issued keys live in-memory only until the persistence shim ships. Single-replica deploys lose them on restart; multi-replica needs sticky LB until then.

---

## Step 2 — (Optional) Wire the tenant's subdomain

Helm chart with `ingress.enabled=true`:

```yaml
ingress:
  enabled: true
  className: nginx
  hosts:
    - host: api.acme.your-domain.com
      paths: [{path: /, pathType: Prefix}]
  tls:
    - hosts: [api.acme.your-domain.com]
      secretName: acme-tls
```

`helm upgrade` and point `api.acme.your-domain.com` at the ingress controller's external IP. Cert-manager handles the TLS.

For Fly.io: `flyctl certs add api.acme.your-domain.com` after a `CNAME` to `<app>.fly.dev`.

---

## Step 3 — Confirm per-org telemetry is flowing

After the tenant does their first fork (have them run the [Customer quickstart](#customer-quickstart) below), check Prometheus:

```promql
sum by (org) (rate(nanovm_forks_total_by_org[5m]))
```

You should see a non-zero series for `org="acme"`. If not:
- `curl https://<endpoint>/metrics | grep acme` — is the series present at all?
- `curl https://<endpoint>/v1/usage/by-org?all=true -H "Authorization: Bearer <operator-tok>"` — does the control plane have a row for them?
- Check the Prometheus scrape job (see `deploy/prometheus/prometheus-scrape.yaml`).

The Grafana dashboard at `deploy/grafana/dashboards/nanovm-overview.json` has a "Top 10 callers" panel that gives you the at-a-glance fleet view.

---

## Step 4 — Pull billing data

End of billing period, query the control plane for the tenant's total spend:

```sh
curl -s "https://<endpoint>/v1/usage/by-org?all=true" \
  -H "Authorization: Bearer <operator-tok>" | jq
```

Response:
```json
{
  "orgs": [
    { "org_id": "acme",   "fork_count": 12453, "fork_total_ms": 168291 },
    { "org_id": "globex", "fork_count":  3120, "fork_total_ms":  41887 }
  ]
}
```

`fork_count` × your-per-fork-price + `fork_total_ms` × your-per-compute-ms-price = invoice line.

For continuous Stripe Metering / Orb push: scrape `/metrics` every 5 min, send the rate-of-counter delta to your billing provider.

> The `?all=true` flag only works for the `default` org (the operator scope). Other orgs see their own row only — safe to hand to tenant dashboards.

---

## Customer quickstart

This is the snippet to send the tenant. Replace `<token>` and `<endpoint>`.

### curl

```sh
# 1. Health check.
curl https://<endpoint>/v1/health -H "Authorization: Bearer <token>" | jq .backend
# → "kvm"

# 2. Create a VM (mock config; replace `kernel` / `rootfs` for real workloads).
curl -X POST https://<endpoint>/v1/vms \
  -H "Authorization: Bearer <token>" \
  -H "content-type: application/json" \
  -d '{"vcpus":1,"memory_mib":256}'
# → { "id": 1, "display": "vm-...", "state": "created" }

# 3. Start + snapshot + fork.
curl -X POST https://<endpoint>/v1/vms/1/start -H "Authorization: Bearer <token>"
curl -X POST https://<endpoint>/v1/vms/1/snapshot -H "Authorization: Bearer <token>"
# → { "id": 1 }
curl -X POST https://<endpoint>/v1/snapshots/1/fork -H "Authorization: Bearer <token>"
# → { "vm": {...}, "fork_ms": 8, "fork_count": 1, "fork_total_ms": 8 }

# 4. Check your own usage at any time.
curl https://<endpoint>/v1/usage/by-org -H "Authorization: Bearer <token>"
# → { "orgs": [{"org_id":"acme","fork_count":1,"fork_total_ms":8}] }
```

### Python SDK

```py
from nanovm import Client

nv = Client("https://<endpoint>", token="<token>")
snap = nv.snapshot_from_running(nv.create_vm(vcpus=1, memory_mib=256).id)
fork = nv.fork(snap.id)
print(fork.fork_ms, "ms")  # headline product number
```

### MCP bridge (Claude / Cursor agents)

```sh
NANOVM_ENDPOINT=https://<endpoint> NANOVM_TOKEN=<token> nanovm-mcp
```

Then add to your agent's MCP config:
```json
{
  "mcpServers": {
    "nanovm": { "command": "nanovm-mcp" }
  }
}
```

---

## Rotating / revoking tenant keys

A tenant rotates their own keys via `/v1/keys`:

```sh
# Issue new.
NEW=$(curl -s -X POST https://<endpoint>/v1/keys -H "Authorization: Bearer <old-token>")
echo "$NEW" | jq -r .token   # persist this

# Revoke old (use the OLD key's id, which the tenant got at issuance).
curl -X DELETE "https://<endpoint>/v1/keys/<old-id>" \
  -H "Authorization: Bearer $(echo "$NEW" | jq -r .token)"
```

For env-loaded bootstrap tokens (`NANOVM_API_TOKENS`), the operator rotates by editing the env and redeploying.

---

## Cutting a tenant off

Operator-side: remove their entry from `NANOVM_API_TOKENS` and redeploy. Their bootstrap token stops being accepted on the next pod's startup. Any runtime-issued keys derived from it also stop working (the chain dies with the bootstrap).

If they only had runtime keys (no env entry), use `DELETE /v1/keys/{id}` per key — requires impersonating their org or the operator-scope.

> Caveat: the in-memory ownership map means VMs they created STAY in the control plane until destroyed or until the process restarts. Run a destroy sweep before the cutoff if you care about freeing host resources.

---

## Common gotchas

- **Per-org metrics show `default` instead of `acme`.** The bootstrap token wasn't `acme:tok` — it landed in the default org. Re-add as `acme:tok` and redeploy.
- **429 fork_quota_exceeded right after onboarding.** Default per-token quota is conservative. Tune via `NANOVM_FORK_QUOTA_PER_SEC` env / chart `config.forkQuotaPerSec`.
- **/v1/usage/by-org returns an empty array for a brand-new tenant.** The metric is populated lazily on first fork. Have them fire one before checking.
- **`/v1/keys` returns 401 when the tenant has only a runtime token.** They lost it on a restart (in-memory only). Mint them a fresh bootstrap via Step 1a until persistence ships.
