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

| # | Scope | Needs KVM host | Status |
| - | --- | --- | --- |
| **M0** | Workspace scaffold, `Hypervisor` trait, `vm-mock` backend, CI without KVM, docs | **no** | ✅ complete |
| M1 | `vm-kvm` boots minimal kernel; serial "hello from guest" | yes | 🔲 next on KVM host |
| M2 | virtio-vsock transport + musl guest agent; `nanovm exec <id> -- echo hi` round-trips | yes | 🔲 partial (wire format + connection types done) |
| M3 | virtio-fs; `nanovm cp file.py <id>:/work/` | yes | 🔲 placeholder |
| M4 | Python / Node run in guest; stdio streaming demo | yes | 🔲 depends M2 |
| M5 | Snapshot + fork via userfaultfd; warm pool; p50 < 50 ms cold start | yes | 🔲 on-disk format done |
| M6 | Control plane REST API; auth; per-sandbox-second metering | **no** | ✅ complete (axum REST + bearer auth + integration tests) |
| M7 | Docs polish + public launch (HN / r/rust / r/MachineLearning) | any | 🔲 |

Stretch: M8 GPU passthrough, M9 multi-node, M10 confidential compute
(SEV-SNP / TDX).

## M0 — workspace scaffold (complete)

- [x] Cargo workspace with 11 crates (2 real implementations, 1 skeleton,
      5 placeholders, proto, cli, control-plane).
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

## M6 — control plane REST API (complete, no KVM needed)

- [x] `control-plane` crate with axum 0.7 REST router.
- [x] Full CRUD: `POST /v1/vms`, `GET /v1/vms`, `GET /v1/vms/:id`,
      `POST /v1/vms/:id/start`, `POST /v1/vms/:id/stop`,
      `POST /v1/vms/:id/snapshot`, `DELETE /v1/vms/:id`,
      `POST /v1/snapshots/:id/restore`, `GET /healthz`.
- [x] Bearer-token auth middleware (`NANOVM_API_TOKENS` env).
- [x] Structured JSON error envelope `{"error":{"code":"...","message":"..."}}`.
- [x] `nanovm-control-plane` binary wrapping `MockHypervisor` (real KVM
      backend wired in M1 via `Arc<dyn Hypervisor>`).
- [x] 22 end-to-end integration tests using `tower::ServiceExt::oneshot`
      (no network, no KVM).

## M2 — partial progress (no KVM needed for wire-format work)

- [x] `virtio-queue`: descriptor table, flag constants, cycle-safe
      `DescriptorChain` iterator with bounds + cycle detection.
- [x] `virtio-vsock`: 44-byte `VsockHeader` parse/serialize, all op codes
      and type codes, well-known CIDs, shutdown flags.
- [x] `virtio-vsock::Connection` state machine: `Closed → Listen/SynSent →
      Established → CloseWait/FinWait → Closed` with credit-based flow
      control fields.
- [x] `guest-agent` binary scaffold: compiles as a static binary, processes
      `Ping` and `Exec` requests over stdin/stdout (vsock wiring deferred
      to M2 on a KVM host).

## M2 — still needs KVM host

- [ ] Wire `virtio-vsock` into the KVM vCPU run loop (eventfd, virtqueue
      consumer, ioeventfd).
- [ ] `guest-agent` reads/writes on `/dev/vsock` and routes requests to
      the full handler set (WriteFile, ReadFile, Stat, Signal, ExecStart
      streaming).
- [ ] `nanovm exec <id> -- echo hi` round-trips end-to-end.

## M1 — needs KVM host

M1 is blocked on `/dev/kvm`. See [`kvm-host.md`](kvm-host.md) for the
cheapest options. Once on a KVM host the steps are:

1. Add `kvm-ioctls`, `vm-memory`, `linux-loader` behind the `kvm` feature.
2. Implement `KvmHypervisor::create_vm` (mmap guest RAM, load bzImage with
   `linux-loader`).
3. Implement `KvmHypervisor::start` (create vCPU, set registers, run loop).
4. Attach a minimal 8250 UART device so the kernel can print to `ttyS0`.
5. Add seccomp-BPF filter to the VMM process.
6. Smoke-test: `cargo run -p cli -- run bzImage` prints "hello from guest".

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

## Next up

**M1 on a KVM host.** See [`kvm-host.md`](kvm-host.md) for the cheapest
options (local Linux, GCP nested virt, AWS bare metal, Hetzner dedicated).

The M2 vsock wiring can overlap with M1 development — once the kernel boots
and a `ttyS0` line appears, plugging in virtio-vsock is the immediate next
step so the agent can receive commands.

## Development without a KVM host

While a KVM host is being sourced, these items advance the project:

1. **Snapshot runtime (M5 prep)** — add `userfaultfd` bindings and the CoW
   page-fault handler to the `snapshot` crate. The on-disk format is done;
   the page-fault interception is the hard part.

2. **virtio-queue ring parsers** — complete the available/used ring, packed
   virtqueue, and guest-memory integration in `virtio-queue`. These are unit-
   testable with synthetic byte slices.

3. **virtio-fs (M3 prep)** — add FUSE in/out message parsing to `virtio-fs`.
   The FUSE kernel protocol is fully documented and testable offline.

4. **cargo-fuzz harnesses** — add fuzzing targets for `virtio-queue`,
   `virtio-vsock`, and `snapshot` parsers. Run locally with `cargo fuzz`.

5. **OpenAPI / Swagger spec** — auto-generate from the `control-plane`
   routes for external consumers and SDK generation.
