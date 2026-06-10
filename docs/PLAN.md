# Rust-nano-vm — roadmap

`rust-nano-vm` is a purpose-built ephemeral code-execution sandbox
microVM for LLM-agent workloads. The wedge against general-purpose
VMMs and ad-hoc container sandboxes is:

- Single-binary all-Rust VMM + guest agent.
- Snapshot-first cold starts via warm pools.
- A first-class **snapshot + fork** primitive that lets agent eval
  pipelines spawn many variants from a base image in milliseconds.

## Target numbers

| Axis                          | Target                              | Baseline                                          |
| ---                           | ---                                 | ---                                               |
| Cold start (p50, warm pool)   | < 50 ms                             | E2B 150–400 ms                                    |
| Snapshot → fork               | < 80 ms / child                     | Firecracker ~125 ms restore; no native fork       |
| Binary size (single binary)   | < 20 MB static musl                 | Multi-binary Go + Firecracker stack               |
| Idle host RAM / sandbox       | < 30 MB via KSM + snapshot sharing  | Firecracker ~5 MB + runtime                       |
| Agent protocol                | open `agent-sandbox-proto` spec     | E2B proprietary SDK                               |

## Architecture

Every backend implements the `vm-core::Hypervisor` trait. Two backends
exist: `vm-kvm` (real, Linux + `/dev/kvm`) and `vm-mock` (in-memory,
test/CI). Consumers (CLI, control plane) program against the trait,
never the concrete backend.

See [`architecture.md`](architecture.md) for the full diagram + narrative.

## Workspace layout

```
crates/
  vm-core/        Hypervisor trait, VmConfig, VmHandle, VmError, SnapshotId
  vm-mock/        In-memory backend
  vm-kvm/         KVM backend
  virtio-fs/      Host↔guest FS
  virtio-vsock/   Host↔guest RPC transport
  snapshot/       On-disk format + userfaultfd CoW runtime
  guest-agent/    Static musl binary running in guest
  control-plane/  axum REST API, auth, quotas, metering
  proto/          Shared agent-sandbox-proto types
  cli/            `nanovm` binary
```

## Current capabilities

- **Hypervisor trait + mock backend** (`vm-core`, `vm-mock`): full
  state-machine, snapshot/fork semantics, exhaustive unit tests.
- **KVM backend** (`vm-kvm`, behind the `kvm` feature):
  - Minimal kernel boot, vCPU run loop, 8250 UART, MMIO virtio bus.
  - Initramfs loading and userspace boot.
  - virtio-vsock device wired into the run loop with IRQ injection.
  - KVM vCPU + machine-state snapshot capture and restore.
  - Snapshot fan-out via `MAP_PRIVATE` CoW on the snapshot memory image.
- **virtio-vsock**: 44-byte header parse/serialize, all op and type
  codes, well-known CIDs, full connection state machine with
  credit-based flow control, split-virtqueue traversal over guest RAM,
  length-prefixed framed transport.
- **virtio-fs**: FUSE protocol framing, per-op body types (24 opcodes),
  dispatch scaffolding with a `FuseHandler` trait, `StdFsHandler`
  backed by `std::fs` covering the common copy path.
- **virtio-queue**: split-ring (avail / used / descriptor) and packed
  virtqueue parsers; guest-memory integration trait.
- **Snapshot format**: on-disk file format, JSON-serialised vCPU and
  machine state, userfaultfd-driven CoW page-fault handler.
- **Guest agent**: static musl `/init` running inside the guest;
  framed RPC over vsock; streaming `exec_in_guest`.
- **Control plane**: axum REST with bearer-token auth, per-token
  token-bucket quota on the expensive `/fork` route, per-caller usage
  metering, Prometheus `/metrics`, OpenAPI 3.1 contract,
  integration-tested against `vm-mock`.
- **CLI**: `nanovm run`, `exec`, `cp`, `snapshot`, `fork`, `ps` driven
  through the control plane.
- **Fuzzing**: `cargo-fuzz` harnesses for `virtio-queue`,
  `virtio-vsock`, `virtio-fs`, and `snapshot` parsers.

## Roadmap

The next pieces of work, roughly in dependency order:

1. **End-to-end exec on a KVM host.** The vsock device is attached and
   the guest agent runs; binding a real `AF_VSOCK` listener and routing
   the full handler set (WriteFile, ReadFile, Stat, Signal, streaming
   ExecStart) closes the round-trip from `nanovm exec` to a real Linux
   guest.
2. **virtio-fs end-to-end.** The dispatch and `StdFsHandler` are
   complete; remaining work is wiring the virtqueue (MMIO interrupt
   and ioeventfd) into the dispatch loop so `nanovm cp` round-trips.
3. **Warm pool + snapshot-fork benchmarks.** The snapshot CoW path
   works; the next milestone is a warm-pool front-end on the control
   plane and a benchmark suite that reports cold-start p50 / fork
   latency / per-fork Pss against the target numbers above.
4. **Seccomp-BPF on the VMM process.** Tightens the host-side syscall
   surface to a Firecracker-equivalent filter.

Stretch (not committed): GPU passthrough, multi-node, confidential
compute (SEV-SNP / TDX).

## Differentiation & non-goals

**Wedge:** the snapshot-fork primitive for agent eval pipelines. This
is the one capability Firecracker and E2B do not offer natively.

**Non-goals (v1):** GPU passthrough, live migration, Windows/macOS
guests, multi-tenant hard SLA, confidential compute. Revisit v2.

## Risks & mitigations

| Risk                                                                | Mitigation                                                                                                |
| ---                                                                 | ---                                                                                                       |
| KVM cannot be exercised from every dev environment                  | `Hypervisor` trait + `vm-mock` keep CI green without `/dev/kvm`; KVM tests gated behind a feature flag    |
| rust-vmm learning curve                                             | Firecracker + Cloud Hypervisor as reference implementations; start from minimal kernel + serial           |
| Adjacent projects ship faster                                       | Stay narrow on the snapshot-fork wedge instead of competing across the whole FaaS surface                 |
| Security posture                                                    | Mirror Firecracker threat model; `cargo-fuzz` on virtio queue parsers; `cargo-deny` advisories in CI      |

## Verification

From the repo root:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo run -p cli -- --help
```

All five commands must succeed without `/dev/kvm`. The KVM-backed
integration tests live behind `cargo test -p vm-kvm --features kvm`
and require a Linux host with `/dev/kvm`.
