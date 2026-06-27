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

```sh
helm install nanovm ./deploy/helm/nanovm \
  --namespace nanovm --create-namespace \
  --set config.apiTokens="acme:$(openssl rand -hex 24),globex:$(openssl rand -hex 24)"
```

Production overrides worth setting:

```yaml
image:
  tag: "0.0.4"  # pin to a released version, never `latest`

config:
  apiTokens: ""           # leave empty; set via SealedSecret / external-secrets
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
