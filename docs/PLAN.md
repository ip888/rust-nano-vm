# Rust-nano-vm — roadmap

> This file is the single source of truth for the project roadmap.
> Committed into the repo so future sessions (or reviewers) can pick up
> the plan without relying on external state.

## Context

`rust-nano-vm` is a purpose-built ephemeral code-execution sandbox microVM
for LLM agents. The wedge against incumbents (E2B on Go+Firecracker,
ad-hoc containers) is:

- Single-binary all-Rust VMM + guest agent.
- Snapshot-first cold starts (< 50 ms via warm pools).
- A first-class **snapshot + fork** primitive that lets agent eval
  pipelines spawn 1000 variants from a base image in seconds.

Open-core business model: OSS core (Apache-2.0 / MIT), managed cloud,
enterprise self-hosted.

### Target numbers

| Axis | Target | Baseline |
| --- | --- | --- |
| Cold start (p50, warm pool) | < 50 ms | E2B 150–400 ms |
| Snapshot → fork | < 80 ms / child | Firecracker ~125 ms restore; no native fork |
| Binary size (single binary) | < 20 MB static musl | Multi-binary Go + Firecracker stack |
| Idle host RAM / sandbox | < 30 MB via KSM + snapshot sharing | Firecracker ~5 MB + runtime |
| Agent protocol | open `agent-sandbox-proto` spec | E2B proprietary SDK |

## Architecture

Every backend implements the `vm-core::Hypervisor` trait. M0 ships two
implementations: `vm-kvm` (real, Linux + /dev/kvm) and `vm-mock` (in-memory,
test/CI). Consumers (CLI, control plane) program against the trait, never
the concrete backend.

See [`architecture.md`](architecture.md) for the full diagram + narrative.

## Workspace layout

```
crates/
  vm-core/        Hypervisor trait, VmConfig, VmHandle, VmError, SnapshotId
  vm-mock/        In-memory backend (M0, complete)
  vm-kvm/         KVM backend (M0 skeleton, M1 real)
  virtio-fs/      Host↔guest FS (M3)
  virtio-vsock/   Host↔guest RPC transport (M2)
  snapshot/       userfaultfd + CoW snapshot/fork (M5)
  guest-agent/    Static musl binary running in guest (M2)
  control-plane/  axum REST/gRPC API, auth, metering (M6)
  proto/          Shared agent-sandbox-proto types (M0, defined)
  cli/            `nanovm` binary, clap subcommands (M0 shell, M1+ real)
```

## Milestones

| # | Scope | Needs KVM host |
| - | --- | --- |
| **M0** | Workspace scaffold, `Hypervisor` trait, `vm-mock` backend, CI without KVM, docs | **no (this session)** |
| M1 | `vm-kvm` boots minimal kernel; serial "hello from guest" | yes |
| M2 | virtio-vsock transport + musl guest agent; `nanovm exec <id> -- echo hi` round-trips | yes |
| M3 | virtio-fs; `nanovm cp file.py <id>:/work/` | yes |
| M4 | Python / Node run in guest; stdio streaming demo | yes |
| M5 | Snapshot + fork via userfaultfd; warm pool; p50 < 50 ms cold start | yes |
| M6 | Control plane lifecycle API on `vm-mock`; quotas, metering, and KVM wiring follow on top | no — foundation already ships without KVM |
| M7 | Docs polish + public launch (HN / r/rust / r/MachineLearning) | any |

Stretch: M8 GPU passthrough, M9 multi-node, M10 confidential compute
(SEV-SNP / TDX).

## M0 — what this session ships

- [x] Cargo workspace with 11 crates (6 implemented foundations, 2 entry
      points, 3 placeholders/skeletons).
- [x] `vm-core::Hypervisor` trait + supporting types (`VmConfig`,
      `VmHandle`, `VmState`, `VmError`, `VmId`, `SnapshotId`).
- [x] `vm-mock::MockHypervisor` with full state-machine + snapshot/fork
      semantics and unit tests covering all transitions.
- [x] `vm-kvm::KvmHypervisor` skeleton; all methods return
      `VmError::Unsupported` until M1; heavy deps gated behind the `kvm`
      feature flag so CI stays fast.
- [x] `proto` crate with `Request`/`Response` envelopes, `RequestBody`,
      `ResponseBody`, `ErrorCode`, and serde roundtrip tests.
- [x] `nanovm` CLI with `run`, `exec`, `cp`, `snapshot`, `fork`, `ps`
      subcommands; each prints "unimplemented: milestone Mx" and exits 2.
- [x] GitHub Actions CI: `fmt --check`, `clippy -D warnings`,
      `test --workspace`, `build --workspace`. No KVM device required.
- [x] Docs: `PLAN.md` (this file), `architecture.md`, `comparison.md`,
      `kvm-host.md`.
- [x] Dual Apache-2.0 / MIT licensing (Rust convention).

## Verification

From the repo root:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo run -p cli -- --help
```

All five commands must succeed without `/dev/kvm`.

## Differentiation & non-goals

**Wedge:** the snapshot-fork primitive for agent eval pipelines. This is
the one capability Firecracker and E2B do not offer natively.

**Non-goals (v1):** GPU passthrough, live migration, Windows/macOS
guests, multi-tenant hard SLA, confidential compute. Revisit v2.

## Risks & mitigations

| Risk | Mitigation |
| --- | --- |
| Cannot test KVM from the dev sandbox (gVisor, no `/dev/kvm`) | `Hypervisor` trait + `vm-mock`; real tests run on a KVM host (see `kvm-host.md`) |
| rust-vmm learning curve | Firecracker + Cloud Hypervisor as reference implementations; start from minimal kernel + serial |
| E2B ships faster | Niche: "the sandbox for agent eval pipelines"; snapshot+fork is the hook |
| Security posture | Mirror Firecracker threat model; `cargo-fuzz` on virtio queue parsers from day 1; RustSec audit in CI |
| Solo burnout | Ship M0–M4 publicly before M5; treat M5 as the v0.1 launch gate |

## Current codebase assessment

The workspace baseline is green on a non-KVM host:

- `cargo build --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all -- --check`

That baseline means the project has already moved beyond a pure scaffold.
Today the strongest foundations are:

- `vm-core`: the stable trait boundary every backend and entry point depends
  on.
- `vm-mock`: a fully tested state-machine backend that keeps CI and API work
  independent from `/dev/kvm`.
- `proto`, `virtio-queue`, `virtio-vsock`, and `snapshot`: wire-format and
  file-format crates with focused serialization and parser coverage.
- `control-plane`: a working REST lifecycle API with auth and integration
  tests against `vm-mock`.

The main gaps are now concentrated in the execution path rather than the
interfaces:

- `vm-kvm` is still an M0 skeleton, so there is no real boot path yet.
- `guest-agent` and `virtio-fs` remain placeholders, so exec/copy workflows
  cannot be exercised end-to-end.
- `cli` intentionally exposes only the command surface; every subcommand still
  exits with a milestone placeholder message.

## Continuation priorities

1. **Finish M1 first: real KVM boot.** This is the critical path because it
   validates the `vm-core` abstraction against a real backend and unblocks all
   later guest-facing work. The minimum useful slice is: open `/dev/kvm`,
   create a VM from `VmConfig`, boot a tiny guest, and make `nanovm run`
   print the serial "hello from guest" path end-to-end.
2. **Then finish M2: vsock + guest-agent exec.** The protocol crate is already
   defined, so the next high-leverage step is wiring `virtio-vsock` into
   `vm-kvm`, implementing a tiny musl `guest-agent`, and making `nanovm exec`
   and the control-plane speak the existing `proto` contract.
3. **Treat snapshot/fork as the first performance milestone, not the first
   implementation milestone.** The `snapshot` crate already pins the on-disk
   format, so the next step there should be a benchmark-backed runtime
   prototype (`userfaultfd`, warm pool, fork latency) once M1/M2 give a real
   guest to snapshot.
4. **Defer ergonomics until the execution path exists.** `virtio-fs`,
   language-runtime images, metering, and quota work are valuable, but they
   should layer on top of a real boot + exec loop rather than compete with it.

## Next up

**M1 on a KVM host.** Use `vm-mock` to preserve fast CI, but do the next round
of implementation work around a single end-to-end KVM success criterion:
create → boot → serial output. See [`kvm-host.md`](kvm-host.md) for the
cheapest options (local Linux, GCP nested virt, AWS bare metal, Hetzner
dedicated).
