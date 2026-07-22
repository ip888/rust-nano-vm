# Enterprise / on-prem install

This directory is for enterprise customers who want to run
`rust-nano-vm` **inside their own network** — private cloud, air-gapped
data center, or a customer's own AWS / GCP / Azure account. It exists
because the SaaS at nanovm.io answers "we run it for you" and the OSS
`README.md` answers "run it on your laptop," but neither covers the
security-questionnaire posture that regulated buyers (finance / health
/ gov / defense) require.

**TL;DR**: use the existing Helm chart under `deploy/helm/nanovm`,
override the `airgap` toggle, mirror the four images below into your
private registry, and read the support-boundary matrix at the bottom
of this file before you sign anything.

## Support-boundary matrix

The single most important thing in a self-hosted deployment is knowing
who owns what when it breaks. This matrix is authoritative — if
something isn't listed as "we own" here, it's on the customer's ops
team.

| Concern | We own (`rust-nano-vm` maintainers) | You own (customer ops) |
|---|---|---|
| Rust binary correctness, KVM driver, seccomp filter, jailer | ✅ | |
| Helm chart + Kubernetes manifests + defaults | ✅ | |
| Container image publishing to `ghcr.io/ip888/*` | ✅ | |
| Image mirroring into your private registry | | ✅ |
| Kubernetes cluster (control plane + node OS + kernel patches) | | ✅ |
| `/dev/kvm` availability on nodes (KVM module, group membership) | | ✅ |
| Persistent-volume backing for `NANOVM_OWNERSHIP_STORE` (SQLite) | | ✅ |
| Persistent-volume backing for JSONL audit log (`NANOVM_AUDIT_LOG`) | | ✅ |
| Reverse-proxy TLS termination (Ingress / gateway) | Chart provides Ingress stub | ✅ (certs, WAF, DDoS) |
| Stripe billing plumbing (`--features billing`) | ✅ (code) | ✅ (Stripe account, secrets, portal URL) |
| SIEM / audit-sink HTTPS collector (`--features audit-sink`) | ✅ (code + sink) | ✅ (collector endpoint, API key) |
| Marketplace snapshot fork (`--features marketplace-fork`) | ✅ (code) | ✅ (`NANOVM_MARKETPLACE_CONFIG`, tarball CDN if any) |
| Observability wiring (Prometheus / Grafana dashboards) | Chart provides `ServiceMonitor`; dashboards under `deploy/grafana/` | ✅ (Prometheus, alertmanager, on-call rotation) |
| Backup + disaster-recovery of the SQLite / audit volumes | | ✅ |
| Guest kernel + rootfs choice, CVE tracking on those images | ✅ (defaults under `tools/*-rootfs/`) | ✅ (image rebuild cadence for the base you ship) |
| CVE response on the Rust binary | ✅ (patch release + security advisory) | ✅ (apply within your SLA) |
| Compliance certifications (SOC 2, ISO 27001, HIPAA BAA) | The Rust code + JSONL audit trail + SIEM sink give you the CONTROLS to pass an audit | ✅ You still commission the actual audit; certifications are yours, not ours |

**No implied SLA on the OSS binary.** A commercial support subscription
covering incident response, patch backports, and a named on-call
contact is negotiated separately — email support@nanovm.io.

## Airgap knob

The `airgap: false` value in `values.yaml` is a **placeholder** for
chart-level network-isolation behavior. The chart does not currently
reference this flag in its templates, so toggling it has no runtime
effect on its own.

The Rust binary already defaults to a safe airgap posture without any
chart wiring:

- The metered-billing reporter is **off by default** — it only activates
  when `NANOVM_BILLING_REPORT_SECS` is explicitly set by the operator.
- Magic-link delivery defaults to logging the verify URL to stdout — no
  outbound email unless `RESEND_API_KEY` and `NANOVM_SIGNUP_FROM` are
  both configured.
- Stripe billing endpoints return `503 billing_disabled` until `STRIPE_*`
  env vars are wired in.

Airgapped operators: simply omit those env vars. The SIEM sink
(`NANOVM_AUDIT_SINK_URL`) is still honored when configured — it targets
whatever URL you provide, including an in-cluster collector.

Marketplace fork tarball URLs must be reachable from inside the private
network — supply your own `NANOVM_MARKETPLACE_CONFIG` with in-cluster
URLs rather than `https://cdn.nanovm.io` paths.

Non-airgap connected deployments (customer running on AWS with public
outbound) can leave `airgap=false` and enable the SaaS-facing bits by
setting the corresponding env vars as needed.

## Pre-pinned images

The four images below are the only network-touching artifacts a
production deployment pulls. Mirror them into your private registry
before the airgap install, then override each `image.repository` in
`values.yaml`.

| Image | Purpose | Digest anchor |
|---|---|---|
| `ghcr.io/ip888/nanovm-control-plane-kvm:0.0.3` | REST server + jailer + vmm-child, KVM-enabled | see `Dockerfile.kvm` |
| `ghcr.io/ip888/nanovm-control-plane:0.0.3` | Same REST server, mock hypervisor only (dev / smoke) | see `Dockerfile` |
| `ghcr.io/ip888/nanovm-web:0.0.3` | Next.js dashboard, distroless-node runtime | see `Dockerfile.web` |
| `ghcr.io/ip888/nanovm-vmlinux:0.0.3` *(optional)* | Prebuilt Firecracker vmlinux + Alpine rootfs, bundled at `/usr/local/share/nanovm/` in the KVM image | see `tools/firecracker-rootfs/` |

**Pin by digest, not tag.** Every image is published with an
immutable SHA-256 digest in the release notes. The example values file
uses tags for readability; production overrides should replace the
`tag:` string with `digest: sha256:...` and drop the tag.

## One-command install

Once images are mirrored:

```bash
helm install nanovm ./deploy/helm/nanovm \
  --set airgap=true \
  --set image.repository=registry.internal.example.com/nanovm-control-plane-kvm \
  --set image.tag=0.0.3 \
  --set config.apiTokens='acme:tok-INITIAL-DO-NOT-COMMIT' \
  --set config.tokenStorePath=/var/lib/nanovm/tokens.json \
  --set config.auditPath=/var/log/nanovm/audit.jsonl
```

The chart uses `emptyDir` for the token-store and audit-log volumes by
default — data is lost on pod restart. For production, replace those
with a PVC by adding custom `volumes` / `volumeMounts` overrides in your
values file (the chart does not currently provide a built-in
`persistence.enabled` toggle). Mount a CSI-backed volume at
`/var/lib/nanovm` and `/var/log/nanovm` to make the token store, the
ownership SQLite, and the JSONL audit log survive pod restarts.

## Feature-flag matrix

The Rust binary is one process, but its cargo features gate specific
enterprise capabilities:

| Feature | Deploy shape | Enables |
|---|---|---|
| `sqlite` | Any multi-tenant | `NANOVM_OWNERSHIP_STORE` — org→VM/snapshot mapping survives restart. Non-negotiable for real multi-tenant. |
| `billing` | SaaS | Stripe signup, billing portal, webhook, metered-usage reporter. Implies `sqlite`. |
| `marketplace-fork` | Customer-facing SaaS or on-prem marketplace | `POST /v1/marketplace/snapshots/:name/fork`. Requires tarballs reachable at the URLs in `NANOVM_MARKETPLACE_CONFIG`. |
| `audit-sink` | Enterprise / regulated | HTTP webhook sink for the audit log — see `docs/enterprise-audit.md`. |
| `s3` | Any snapshot-heavy deploy | S3 snapshot store backend. Alt: `file://` for on-cluster PVC. |

Recommended enterprise builds:
- **Regulated on-prem** (no Stripe, yes SIEM): `--features sqlite,audit-sink,marketplace-fork`
- **Full SaaS**: `--features billing,marketplace-fork,audit-sink,s3`
- **Bare on-prem lab**: `--features sqlite` (everything else default)

To enable feature-gated endpoints, rebuild from source with
`--features <list>` (e.g., `cargo build --release --features billing,audit-sink`).
`Dockerfile.kvm` does not accept a `CARGO_FEATURES` build argument, and
the published images do not enable optional features — the `cargo build`
invocations in both `Dockerfile` and `Dockerfile.kvm` run without
`--features`. Operators who need `billing`, `audit-sink`, or other
feature-gated surfaces must build and publish their own image.

## Audit + observability

Both are covered in dedicated docs:
- [`docs/enterprise-audit.md`](../../docs/enterprise-audit.md) — JSONL
  file appender + SIEM webhook sink, record shape, SOC 2 / HIPAA /
  ISO 27001 posture, Datadog + Splunk HEC examples.
- [`deploy/grafana/`](../grafana/) — pre-built dashboards for
  fork-rate, warm-pool hit-rate, dunning-block counter, per-org
  usage.

## Security posture (what auditors ask for)

- **Rust `#![forbid(unsafe_code)]`** on all control-plane crates
  (`control-plane`, `nanovm-jailer`, and shared libraries). The
  `vm-kvm` crate is explicitly exempt — it requires `unsafe` for the
  KVM ioctl ABI. Verifiable in-tree.
- **Seccomp deny-list** on the vmm-child, cgroups on the jailer.
- **Bearer tokens fingerprinted in logs** (never the raw secret).
- **RFC 3339 timestamps** on every audit record; SIEM sink preserves
  the same shape for correlation.
- **HMAC-SHA256 webhook verification** on the Stripe endpoint (when
  `billing` is on).
- **Distroless runtime image** (`gcr.io/distroless/*:nonroot`) — no
  shell, no package manager, no unrelated userland inside the container.
- **`readOnlyRootFilesystem: true`** in the chart's default
  `containerSecurityContext`; only the audit + snapshot volumes are
  writable.

## SBOM / provenance

Every published image includes:
- `sbom.spdx.json` attached as an OCI artifact (Syft-generated at build).
- `cosign` signature (keyless, GitHub OIDC).
- Provenance attestation (SLSA v1 build type).

Verify with:
```bash
cosign verify ghcr.io/ip888/nanovm-control-plane-kvm:0.0.3 \
  --certificate-identity=https://github.com/ip888/rust-nano-vm/.github/workflows/release.yml@refs/tags/v0.0.3 \
  --certificate-oidc-issuer=https://token.actions.githubusercontent.com
```

## Getting help

- **Bug reports / feature requests**: GitHub issues at
  https://github.com/ip888/rust-nano-vm.
- **Commercial support** (guaranteed response times, patch backports,
  named on-call): email support@nanovm.io.
- **Security vulnerabilities**: security@nanovm.io — please DO NOT
  file public GitHub issues for security-impacting bugs; use the
  private disclosure channel first. Standard 90-day disclosure with
  coordinated release.
