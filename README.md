# Rust-nano-vm

> A purpose-built ephemeral code-execution sandbox microVM for LLM agents.

[![License: Apache-2.0 OR MIT](https://img.shields.io/badge/license-Apache--2.0_OR_MIT-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.94+-orange.svg)](rust-toolchain.toml)

**Status:** pre-alpha. Scaffolding in progress (milestone **M0**). See [`docs/PLAN.md`](docs/PLAN.md) for the roadmap.

## Why

Every AI coding agent — Claude Code, Cursor, Devin, OpenHands, aider,
autogen, eval harnesses — needs to run generated code somewhere safe.
Today the options are:

- **E2B** (Go + Firecracker) — 150–400 ms cold start, closed managed service.
- **Containers** — weak isolation, slow for frequently-forked workloads.
- **Firecracker** directly — fast, but general-purpose serverless, not
  tuned for short-lived I/O-heavy agent workloads.

`rust-nano-vm` is the underserved niche: a single-binary Rust VMM + guest
agent, snapshot-first, with a first-class **snapshot + fork** primitive so
agent eval pipelines can spawn 1000 variants from a base image in seconds.

## Targets

| Axis | `rust-nano-vm` target | Reference |
| --- | --- | --- |
| Cold start (p50, warm pool) | **< 50 ms** | E2B 150–400 ms |
| Snapshot → fork | **< 80 ms / child** | Firecracker ~125 ms restore; no native fork |
| Binary size (all-in-one) | **< 20 MB** static musl | Multi-binary Go + Firecracker stack |
| Idle host memory / sandbox | **< 30 MB** via KSM + snapshot sharing | Firecracker ~5 MB + runtime |
| Agent protocol | open `agent-sandbox-proto` spec | E2B proprietary SDK |

See [`docs/comparison.md`](docs/comparison.md) for detailed head-to-head.

## Quickstart

> **M0 only ships the workspace scaffold and a mock backend.** Real guest
> boot requires a KVM host and lands in M1 (see [`docs/kvm-host.md`](docs/kvm-host.md)).

```sh
git clone https://github.com/ip888/Rust-nano-vm.git
cd Rust-nano-vm
cargo build --workspace
cargo test --workspace
cargo run -p cli -- --help
```

Run the REST control plane (M6) against the mock backend — no KVM needed:

```sh
cargo run -p control-plane
# → nanovm-control-plane listening on 127.0.0.1:8080

curl -X POST localhost:8080/v1/vms -H 'content-type: application/json' -d '{}'
# → {"id":1,"display":"vm-0000000000000001","state":"created"}

curl -X POST localhost:8080/v1/vms/1/start  # 204
curl localhost:8080/v1/vms/1                # {"state":"running",...}
```

On a Linux host with `/dev/kvm` (M1+):

```sh
cargo run -p cli --features kvm -- run examples/hello-guest
# → hello from guest
```

## Architecture

```
 ┌─────────────────────────────────────────────────────┐
 │  nanovm CLI  /  control-plane (axum REST + gRPC)    │
 └───────────────┬─────────────────────────────────────┘
                 │ agent-sandbox-proto (serde / JSON-RPC)
                 ▼
 ┌─────────────────────────────────────────────────────┐
 │  vm-core :: trait Hypervisor                        │
 ├─────────────────────────────────────────────────────┤
 │  vm-kvm (real)        │  vm-mock (tests / CI)       │
 │  kvm-ioctls, vm-memory│  in-memory state machine    │
 └────────┬──────────────┴──────────────────────────────┘
          │
          ▼
 ┌─────────────────────────────────────────────────────┐
 │  virtio-vsock │ virtio-fs │ snapshot (userfaultfd)  │
 └─────────────────────────────────────────────────────┘
          │
          ▼
 ┌─────────────────────────────────────────────────────┐
 │  guest-agent (static musl)                          │
 │  exec, fs, signals, stdio streaming                 │
 └─────────────────────────────────────────────────────┘
```

Full narrative in [`docs/architecture.md`](docs/architecture.md).

## Workspace layout

```
crates/
  vm-core/        Hypervisor trait, VmConfig, VmHandle, VmError
  vm-mock/        In-memory backend, no KVM required (used by CI)
  vm-kvm/         KVM backend (feature-gated, Linux-only)
  virtio-fs/      Host↔guest FS  (M3)
  virtio-vsock/   Host↔guest RPC (M2)
  snapshot/       userfaultfd + CoW snapshot/fork (M5)
  guest-agent/    Static musl binary running in guest (M2)
  control-plane/  axum REST API, auth, quotas, metering (M6)
  proto/          Shared agent-sandbox-proto types
  cli/            `nanovm` binary
```

## Milestones

| # | Scope | Needs KVM |
| - | --- | --- |
| M0 | Workspace scaffold, `Hypervisor` trait, mock backend, CI | no |
| M1 | `vm-kvm` boots minimal kernel, serial "hello from guest" | yes |
| M2 | virtio-vsock + musl guest agent, `nanovm exec` round-trip | yes |
| M3 | virtio-fs host↔guest file push/pull | yes |
| M4 | Python/Node in guest, stdio streaming demo | yes |
| M5 | Snapshot + fork; < 50 ms p50 cold start on warm pool | yes |
| M6 | Control plane REST API (lifecycle) | no — runs on `vm-mock`; auth + metering follow on KVM |
| M7 | Public docs + launch | any |

Full plan: [`docs/PLAN.md`](docs/PLAN.md).

## Contributing

Pre-alpha, expect churn. File issues first; large PRs without prior
discussion are likely to be redirected. See [`docs/architecture.md`](docs/architecture.md)
for the trait boundaries — keep all KVM code behind `vm-kvm` and test
against `vm-mock`.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option. Contributions are accepted under the same dual license.
