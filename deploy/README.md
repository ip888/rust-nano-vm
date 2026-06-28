# Deploying nanovm-control-plane

Three production-shaped paths, in order of how fast you can stand one
up. All three serve the same REST surface on `:8080` and expect the
same `NANOVM_API_TOKENS` env shape.

| Target           | Time-to-first-VM | Where the host gets `/dev/kvm`          |
|------------------|------------------|------------------------------------------|
| Fly.io           | ~5 min           | Fly Machines KVM (`compute_kind = kvm`)  |
| Kubernetes (Helm)| ~15 min          | Node-level `/dev/kvm`, mounted in pod    |
| AWS Nitro        | ~10 min (per VM) | Bare-metal Nitro instance, native KVM    |

> The default `Dockerfile` builds the mock backend (no `/dev/kvm`
> needed). For real microVMs use `Dockerfile.kvm` — pushed as
> `ghcr.io/ip888/nanovm-control-plane-kvm:<version>`. All three deploy
> paths below use the KVM image.

---

## 1. Fly.io (fastest)

Fly's `kvm` compute kind exposes the device directly to the VM your
container runs in. No additional plumbing needed.

```bash
fly launch \
  --image ghcr.io/ip888/nanovm-control-plane-kvm:0.0.3 \
  --vm-cpu-kind=performance \
  --vm-memory=2048
fly secrets set NANOVM_API_TOKENS=acme:$(openssl rand -hex 16)
fly deploy
```

`fly.toml` minimum:

```toml
app = "your-app-name"

[build]
image = "ghcr.io/ip888/nanovm-control-plane-kvm:0.0.3"

[[vm]]
compute_kind = "kvm"
size = "performance-2x"
memory = "2gb"

[[services]]
internal_port = 8080
protocol = "tcp"
[[services.ports]]
port = 443
handlers = ["tls", "http"]
```

Verify:

```bash
curl https://your-app-name.fly.dev/healthz
# → ok
curl -H "Authorization: Bearer <token>" \
     https://your-app-name.fly.dev/v1/health
# → {"ok":true,"backend":"kvm",...}
```

---

## 2. Kubernetes (Helm)

For long-lived deployments where you want HA, observability, and
proper secrets management. The chart at `deploy/helm/nanovm` wires
`/dev/kvm` into the pod via `hostPath` (default) or a device plugin,
pins to KVM-capable nodes via `nodeSelector`, and optionally enables a
Prometheus Operator `ServiceMonitor`.

### Prereqs

- A Kubernetes cluster whose nodes expose `/dev/kvm`. Bare metal works
  out of the box; cloud nodes need nested-virt capable instance
  families (AWS `*.metal`, GCP `n2`/`c3` with the nested-virt licence,
  Azure `D...s_v5`, …).
- A node label `nanovm.io/kvm=true` on each KVM-capable node:
  ```bash
  kubectl label node <node> nanovm.io/kvm=true
  ```
  (or change `nodeSelector` in `values.yaml`).

### Install

```bash
helm install nanovm ./deploy/helm/nanovm \
  --create-namespace --namespace nanovm \
  --set apiTokens="acme:$(openssl rand -hex 16)"
```

### Bring your own Secret

Production setups usually want the bearer tokens managed by
Sealed-Secrets, External Secrets Operator, or similar. Create the
Secret out-of-band and point the chart at it:

```bash
kubectl -n nanovm create secret generic nanovm-tokens \
  --from-literal=NANOVM_API_TOKENS="acme:$(openssl rand -hex 16)"
helm install nanovm ./deploy/helm/nanovm \
  --namespace nanovm \
  --set existingSecret=nanovm-tokens
```

### Verify

```bash
kubectl -n nanovm port-forward svc/nanovm 8080:8080
curl http://localhost:8080/healthz
# → ok
```

### Scaling

`replicaCount > 1` works for the read path and for the mock backend,
but **runtime-issued API keys** (`POST /v1/keys`) are currently
per-replica in-memory. Multi-replica needs either a sticky load
balancer or the (planned) shared token store. Until then, keep
`replicaCount: 1` for any cluster where customers will be self-serving
keys.

---

## 3. AWS Nitro bare-metal (most control)

Nitro instance families (`m5.metal`, `c6i.metal`, `i4i.metal`, …)
expose `/dev/kvm` natively. Use Docker (or systemd) on the host:

```bash
sudo docker run -d \
  --name nanovm \
  --restart=always \
  --device=/dev/kvm \
  -p 8080:8080 \
  -e NANOVM_API_TOKENS="acme:$(openssl rand -hex 32)" \
  -e NANOVM_AUDIT_LOG=/var/lib/nanovm/audit.jsonl \
  -v /var/lib/nanovm:/var/lib/nanovm \
  ghcr.io/ip888/nanovm-control-plane-kvm:0.0.3
```

Front it with an ALB / NLB for TLS termination + the usual cloud
plumbing. The container itself doesn't terminate TLS.

---

## Verifying KVM is wired correctly

Whichever path you took, this curl should report `backend: "kvm"`:

```bash
curl -H "Authorization: Bearer <token>" \
     http(s)://<endpoint>/v1/health
# {"ok":true,"backend":"kvm","version":"0.0.3","uptime_secs":42,...}
```

If `backend` is `"mock"` you booted the wrong image; if `/v1/health`
returns 500 with `backend: /dev/kvm not accessible`, the device isn't
reachable from inside the pod / container.

## Observability

`/metrics` is exposed unauthenticated by design (matches the
Prometheus exporter convention). Either keep the listener on a private
network OR add a reverse-proxy ACL. The chart's optional
`serviceMonitor.enabled=true` flips on Prometheus Operator scraping.

### Logs

Set `NANOVM_LOG_FORMAT=json` on every reachable deployment. The binary
then emits newline-delimited JSON tracing events that drop straight
into Loki / Datadog / CloudWatch / OpenSearch without a regex parser
in between. Levels are still controlled by `RUST_LOG` (default
`info`).

### Prometheus + Grafana

Drop-in configs live under `deploy/prometheus/` and `deploy/grafana/`:

| File                                            | Purpose                                       |
|-------------------------------------------------|-----------------------------------------------|
| `deploy/prometheus/prometheus-scrape.yaml`      | Static + Kubernetes-SD scrape job for nanovm  |
| `deploy/prometheus/alerts.yaml`                 | 4 production alerts (down / latency / pool / throttle) |
| `deploy/grafana/dashboards/nanovm-overview.json`| Operator-facing dashboard with the headline 6 panels |

The Grafana dashboard expects a Prometheus datasource and a `job`
template variable that defaults to `nanovm`. Import it via Grafana →
Dashboards → New → Import → upload JSON.

The alerting rules assume a scrape `job_name: nanovm` (matches the
example scrape config). Rename if yours differs.

### Metrics reference

Series the control plane exposes today:

| Metric                                       | Type    | Labels   | Description                                        |
|----------------------------------------------|---------|----------|----------------------------------------------------|
| `nanovm_up`                                  | gauge   | —        | Always `1` while the process is serving.           |
| `nanovm_forks_total`                         | counter | `token`  | Successful forks. `token` is a non-secret fingerprint. |
| `nanovm_fork_latency_ms_sum`                 | counter | —        | Sum of per-fork wall-time (ms).                    |
| `nanovm_fork_latency_ms_count`               | counter | —        | Number of latency observations.                    |
| `nanovm_fork_quota_throttled_total`          | counter | `token`  | Forks rejected by per-token quota.                 |
| `nanovm_warm_pool_hits_total`                | counter | —        | Forks served from the warm pool.                   |
| `nanovm_warm_pool_misses_total`              | counter | —        | Forks that fell through to a cold restore.         |

Avg fork latency = `rate(sum) / rate(count)`. Hit rate = `hits / (hits + misses)`.
