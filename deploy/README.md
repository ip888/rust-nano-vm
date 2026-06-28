# Deploying `nanovm-control-plane`

Three production paths, ordered by setup cost.

## Path 1 — Fly.io (~5 minutes)

Fly.io's performance machines expose `/dev/kvm` directly. Cheapest way to get a real-KVM nanovm online today.

```sh
flyctl launch --image ghcr.io/ip888/nanovm-control-plane-kvm:latest \
  --name nanovm-acme \
  --vm-cpu-kind performance \
  --vm-memory 4096 \
  --region iad
flyctl secrets set NANOVM_API_TOKENS="acme:$(openssl rand -hex 24)"
flyctl deploy
```

Confirm `/dev/kvm` is available and the backend self-reports as `kvm`:

```sh
flyctl ssh console -C 'curl -fs http://127.0.0.1:8080/v1/health \
  -H "Authorization: Bearer <your-token>" | jq .backend'
# → "kvm"
```

## Path 2 — Kubernetes via Helm (production shape)

Works on any cluster whose nodes expose `/dev/kvm`. Tested on:
- AWS EKS with `m5.metal` / `c5.metal` node groups
- GKE with `c2-standard-*` nodes (nested virt opt-in)
- on-prem bare metal with the KVM kernel module loaded

### One-time node setup

Label the KVM-capable nodes so the chart's `nodeSelector` finds them:

```sh
kubectl label node <node-name> nanovm.io/kvm=true
```

If your nodes use a non-standard `kvm` group GID (Debian = 36, RHEL = 78), set it in your override:

```yaml
podSecurityContext:
  runAsGroup: 36   # or 78 for RHEL-family
  fsGroup: 36
```

### Install

The chart **requires** either `config.apiTokens` or `tokensSecret.existingSecret` to be set — it won't render an auth-disabled deployment by default. Two shapes:

```sh
# Inline tokens (fine for dev / first-boot).
helm install nanovm ./deploy/helm/nanovm \
  --namespace nanovm --create-namespace \
  --set config.apiTokens="acme:$(openssl rand -hex 24),globex:$(openssl rand -hex 24)"

# Bring-your-own Secret (recommended for prod: managed by sealed-secrets / ExternalSecrets / Vault).
kubectl -n nanovm create secret generic nanovm-tokens \
  --from-literal=NANOVM_API_TOKENS="acme:$(openssl rand -hex 24)"
helm install nanovm ./deploy/helm/nanovm \
  --namespace nanovm --create-namespace \
  --set tokensSecret.existingSecret=nanovm-tokens
```

To deliberately run auth-off on a throwaway cluster, set `config.apiTokens=NONE` (literal string).

Production overrides worth setting:

```yaml
image:
  tag: "0.0.3"  # pin to a released version, never `latest`

# Tokens managed out-of-band — chart doesn't render its own Secret.
tokensSecret:
  existingSecret: nanovm-tokens

config:
  forkQuotaPerSec: 50
  warmPoolPerSnapshot: 8
  snapshotStore: "s3://acme-nanovm-snapshots/prod"

serviceMonitor:
  enabled: true           # if you run Prometheus Operator

ingress:
  enabled: true
  className: nginx
  hosts:
    - host: api.nanovm.acme.com
      paths: [{path: /, pathType: Prefix}]
  tls:
    - hosts: [api.nanovm.acme.com]
      secretName: nanovm-tls
```

### Verify

```sh
kubectl -n nanovm port-forward svc/nanovm 8080:8080 &
curl -fs http://127.0.0.1:8080/v1/health \
  -H "Authorization: Bearer <acme-token>" | jq .backend
# → "kvm"
```

## Path 3 — AWS Nitro bare-metal (`*.metal`)

Use this when you want a single-tenant deploy without K8s overhead.

```sh
# 1. Spin up an m5.metal (or c5.metal, c6id.metal, ...).
# 2. Confirm /dev/kvm is exposed:
ls -l /dev/kvm
# crw-rw---- 1 root kvm 10, 232 Jun 27 ... /dev/kvm

# 3. Install docker (or podman) + the nanovm image:
sudo apt-get install -y docker.io
sudo docker pull ghcr.io/ip888/nanovm-control-plane-kvm:latest

# 4. Run with /dev/kvm mapped through:
sudo docker run -d --restart unless-stopped \
  --name nanovm \
  --device /dev/kvm \
  -p 8080:8080 \
  -e NANOVM_API_TOKENS="acme:$(openssl rand -hex 24)" \
  ghcr.io/ip888/nanovm-control-plane-kvm:latest
```

systemd unit for the same: `deploy/systemd/nanovm.service` (TODO follow-up).

## What gets shipped

| Component | Image | Use case |
|---|---|---|
| `Dockerfile` | `ghcr.io/ip888/nanovm-control-plane` | mock backend, dev / smoke tests |
| `Dockerfile.kvm` (this PR) | `ghcr.io/ip888/nanovm-control-plane-kvm` | production, real KVM |

The Helm chart defaults to the KVM image; switch the `image.repository` to the plain repository if you want the mock backend in cluster (useful for staging clusters without `/dev/kvm`).

## Honest non-features

- **No auto-scaling.** The control plane is stateful (VM ids, snapshot ids, ownership map are all in-memory today). A multi-replica deployment is HA-blast-radius only — pin clients to one replica via session affinity if you need stable ids.
- **No persistence.** Snapshots live on disk until you set `NANOVM_SNAPSHOT_STORE=s3://...` to push to a durable store. Per-org ownership is in-memory — a restart resets the binding (every resource falls back to `OrgId::default_org()`). A SQLite backing for ownership lands in a follow-up PR.
- **No cross-region replication.** S3 snapshot store enables cross-region restore, but the control plane itself is single-region per deployment.

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
| `nanovm_forks_total_by_org`                  | counter | `org`    | Successful forks bucketed by caller org (PR-A2).   |
| `nanovm_fork_latency_ms_sum`                 | counter | —        | Sum of per-fork wall-time (ms).                    |
| `nanovm_fork_latency_ms_count`               | counter | —        | Number of latency observations.                    |
| `nanovm_fork_latency_ms_sum_by_org`          | counter | `org`    | Per-org fork latency sum (PR-A2).                  |
| `nanovm_fork_quota_throttled_total`          | counter | `token`  | Forks rejected by per-token quota.                 |
| `nanovm_fork_quota_throttled_total_by_org`   | counter | `org`    | Per-org throttle counts (PR-A2).                   |
| `nanovm_warm_pool_hits_total`                | counter | —        | Forks served from the warm pool.                   |
| `nanovm_warm_pool_misses_total`              | counter | —        | Forks that fell through to a cold restore.         |

Avg fork latency = `rate(sum) / rate(count)`. Hit rate = `hits / (hits + misses)`.
