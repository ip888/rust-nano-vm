# rust-nano-vm

> A single-binary Rust microVM for AI-agent code execution.
> **~12 ms cold start. ~0.5 MiB private memory per fork. Thousands of
> concurrent sandboxes per 16 GiB of host RAM.**

[![License: Apache-2.0 OR MIT](https://img.shields.io/badge/license-Apache--2.0_OR_MIT-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.94+-orange.svg)](rust-toolchain.toml)

## Headline numbers

Measured on a stock i5 laptop, 8 GiB RAM, KVM, vanilla Linux. Reproduce
with `cargo run -p bench --features kvm --release --bin nanovm-fork-bench
-- --forks 100 --alive 50`.

| Metric | Value | How |
| --- | --- | --- |
| **Cold start, p50** | **~12 ms** | snapshot → fork (vs E2B 150–400 ms, Firecracker ~125 ms) |
| **Cold start, p99** | **~16 ms** | 100 sequential forks of one snapshot |
| **Per-fork private memory (Pss)** | **~0.5 MiB** at N=50 | `MAP_PRIVATE` CoW + `/proc/self/smaps_rollup` accounting |
| **Shared-page savings** | **>90%** | golden image pages stay shared until written |
| **Density on a 16 GiB host** | **~30 000 concurrent forks** | for minimal-footprint guests; scales with guest dirty-set |
| **Binary size (VMM + control plane)** | **< 8 MiB** stripped | single Rust binary, no jailer |

Per-fork Pss *decreases* as fan-out grows — the marginal cost of fork #50
is lower than fork #10 because the kernel keeps reusing the same
read-only pages.

**Warm pool (opt-in).** Set `NANOVM_WARM_POOL_PER_SNAPSHOT=N` to keep
`N` pre-restored children per snapshot in a background queue. The
customer-visible `/v1/snapshots/:id/fork` call then pops a warmed VM
instead of paying the full KVM restore, collapsing fork latency to a
queue pop plus the HTTP round-trip. Off by default. End-to-end design
+ tests live in
[`crates/control-plane/src/warm_pool.rs`](crates/control-plane/src/warm_pool.rs).
The real-KVM warm vs cold numbers aren't pinned in the table yet
because they're machine-dependent — run the bench yourself:

```sh
# Cold-only (default): the headline ~12 ms p50.
cargo run -p bench --features kvm --release --bin nanovm-fork-bench -- \
    --forks 100

# Cold + warm-pool side-by-side: bench prints both phases and the speedup.
cargo run -p bench --features kvm --release --bin nanovm-fork-bench -- \
    --forks 100 --warm-pool 8
```

## Why this exists

Every AI coding agent — Claude Code, Cursor, Devin, OpenHands, aider,
SWE-bench-style evals — needs to run generated code somewhere safe and
*cheaply*. The current options force a bad trade-off:

- **E2B** — 150–400 ms cold start, closed managed service, per-second
  billing on someone else's hardware.
- **Firecracker directly** — fast (~125 ms restore) but a general-purpose
  serverless VMM; no native fork; you build your own control plane,
  jailer, agent, and snapshot fan-out yourself.
- **Containers** — weak isolation; namespaces share a kernel with the
  attacker's untrusted code.

`rust-nano-vm` is the underserved middle: a **single-binary Rust
VMM + guest agent + REST control plane**, snapshot-first, with
**snapshot → fork as a first-class primitive** so an eval pipeline can
spawn 1000 variants of a base image at ~12 ms each, and the kernel itself
keeps the shared 6–7 MiB golden image *actually shared* across all of
them via `MAP_PRIVATE` copy-on-write.

## What's special

1. **Cold start is an `mmap` away.** Fork doesn't re-boot a kernel; it
   maps the snapshot's memory file `MAP_PRIVATE` and lets the kernel
   serve the read-only golden pages to every child. The whole "trick" is
   ~50 lines of `unsafe` in [`crates/vm-kvm/src/vmstate.rs`](crates/vm-kvm/src/vmstate.rs).
   See [`docs/blog/01-mmap-private.md`](docs/blog/01-mmap-private.md).

2. **Faithful KVM snapshot/restore in <1000 lines.** Full vCPU + LAPIC +
   FPU + MSR + IRQCHIP + PIT capture, JSON-serialized via
   `kvm-bindings`'s `serde` feature, with the guest RAM in a separate
   backing file so you can map it `MAP_PRIVATE` on restore.
   See [`docs/blog/02-snapshot-restore.md`](docs/blog/02-snapshot-restore.md).

3. **Honest accounting.** The bench reports **Pss** (proportional set
   size, read from `/proc/self/smaps_rollup`), not RSS. RSS
   double-counts shared pages and overstates fork cost by 5–10×.

4. **A production-shaped control plane.** Bearer-token auth, per-token
   token-bucket quota on the expensive `/fork` route, per-caller usage
   metering, Prometheus `/metrics` endpoint. ~330 lines of axum, no magic.

5. **Warm pool for steady-state fan-out.** When the eval loop will
   fork the same snapshot thousands of times, opt in to a pre-restored
   pool so each customer fork is a queue pop, not a fresh KVM restore.
   Lazy-warming, bounded refill, drains on snapshot delete. See
   [`crates/control-plane/src/warm_pool.rs`](crates/control-plane/src/warm_pool.rs).

6. **Seccomp-BPF sandbox on the VMM process (opt-in).** Set
   `NANOVM_SECCOMP=1` and the host-side VMM that owns `/dev/kvm`
   refuses a deny-list of escape-pivot syscalls (`execve`,
   `ptrace`, `mount`, `kexec_load`, `init_module`, `setns`,
   `unshare`, `chroot`, `pivot_root`, …) — a KVM escape that
   landed in our address space is now killed via `SIGSYS` at the kernel
   instead of free to spawn shells. Same `seccompiler` crate
   Firecracker uses. See
   [`crates/vm-kvm/src/seccomp.rs`](crates/vm-kvm/src/seccomp.rs).

7. **Cgroups v2 resource caps on the VMM process (opt-in).** Set
   `NANOVM_VMM_MEMORY_LIMIT_MIB=512` and/or `NANOVM_VMM_CPU_QUOTA_PCT=75`
   and the VMM moves itself into a fresh cgroup v2 child with the
   requested `memory.max` / `cpu.max` applied. A runaway guest (fork
   bomb, OOM allocator loop, busy-spin vCPU) trips the kernel's cap
   instead of taking the host with it. **v1 scope: process-wide,
   not per-VM** — every VM hosted by the VMM shares the budget. Per-VM
   caps land in the **per-VM cgroup isolation arc** below — see the
   roadmap table — and use `nanovm-jailer` to give every VM its own
   `memory.max` + `cpu.max`. For the monolithic VMM, the process-wide
   knob still bounds blast radius on the host. Requires cgroup v2;
   only the controller(s) you actually request need to be delegated
   to the VMM's parent (memory-only configs don't need cpu, and vice
   versa). Under systemd: `Delegate=memory`, `Delegate=cpu`, or
   `Delegate=memory cpu` depending on which knobs you set. See
   [`crates/vm-kvm/src/cgroups.rs`](crates/vm-kvm/src/cgroups.rs)
   for the process-wide path and
   [`crates/nanovm-jailer/`](crates/nanovm-jailer/) for the per-VM
   shim.

### Per-VM cgroup isolation arc (in progress)

Splits the monolithic VMM into a process-fleet model so each VM gets
its own cgroup and its own crash domain. Shipping in 6 small PRs:

| PR | Crate / change                       | Status                             |
|----|--------------------------------------|------------------------------------|
| 1  | `vmm-ipc` — wire contract            | ✅ merged (#115)                   |
| 2  | `nanovm-vmm-child` — single-VM worker | ✅ merged (#116)                  |
| 3  | `nanovm-jailer` — per-VM cgroup + execve | ✅ merged (#117)               |
| 4  | `nanovm-fleet` — process-fleet `Hypervisor` impl | ✅ merged                |
| 5  | cross-worker restore + pre-warmed VMM-process pool | this PR              |
| 6  | flip default + delete in-process path | final                             |

8. **Durable snapshots on S3 / MinIO / R2.** Snapshots aren't
   pinned to one host: `POST /v1/snapshots/:id/export` uploads
   a snapshot directory to whatever store you point
   `NANOVM_SNAPSHOT_STORE` at (`s3://bucket/prefix`,
   `file:///var/lib/nanovm/snapshots`), and `POST /v1/snapshots/import`
   pulls it back into any other control-plane process — same
   on-disk layout, portable. Built on the official AWS Rust SDK
   so production IAM roles, instance profiles, and web-identity
   tokens (IRSA) work out of the box; the `--features s3` build
   picks up the dependency only when an operator wants it.
   See [`crates/snapshot/src/store.rs`](crates/snapshot/src/store.rs)
   and [`crates/snapshot/src/s3.rs`](crates/snapshot/src/s3.rs).

9. **No detours.** Custom `virtio-vsock` (~1200 lines), hand-rolled
   Prometheus exposition (no `prometheus` crate dependency),
   `MockHypervisor` for tests so CI doesn't need `/dev/kvm`. Single
   workspace, `cargo test --workspace` green without root.

## Supported platforms

The mock backend (used by tests, the demo, and the control plane's
default wiring) is portable Rust and runs on any platform with a Rust
toolchain. The real `vm-kvm` backend currently targets Linux x86_64;
Linux aarch64 (Graviton / Ampere / Apple Silicon under Linux VMs) is
planned.

| Target | Mock backend | `vm-kvm` (real KVM) | Prebuilt binary |
| --- | --- | --- | --- |
| `x86_64-unknown-linux-gnu`  | ✅ | ✅ | ✅ |
| `aarch64-unknown-linux-gnu` | ✅ | planned | ✅ |
| `aarch64-apple-darwin`      | ✅ | n/a (macOS has no `/dev/kvm`) | ✅ |
| `x86_64-apple-darwin`       | ✅ | n/a | build from source |
| `x86_64-pc-windows-msvc`    | ✅ | n/a | build from source |

## Install — prebuilt binaries

From the [latest release](https://github.com/ip888/Rust-nano-vm/releases/latest),
pick the tarball matching your host. Each one contains
`nanovm-control-plane` and `nanovm` plus README + LICENSE files.

```sh
# Linux x86_64
curl -L https://github.com/ip888/Rust-nano-vm/releases/latest/download/rust-nano-vm-VERSION-x86_64-unknown-linux-gnu.tar.gz | tar xz

# Linux aarch64 (Graviton / Ampere / Oracle A1)
curl -L https://github.com/ip888/Rust-nano-vm/releases/latest/download/rust-nano-vm-VERSION-aarch64-unknown-linux-gnu.tar.gz | tar xz

# macOS Apple Silicon (M1/M2/M3/M4)
curl -L https://github.com/ip888/Rust-nano-vm/releases/latest/download/rust-nano-vm-VERSION-aarch64-apple-darwin.tar.gz | tar xz
```

Replace `VERSION` with the release tag (e.g. `0.0.2`). Each tarball
ships a sidecar `.sha256` for integrity verification.

## Install — Docker (no toolchain needed)

A container image of `nanovm-control-plane` is published to GHCR on
every `v*.*.*` release tag. Built from the same source as the prebuilt
tarballs; distroless runtime (~22 MiB image, no shell, runs as the
`nonroot` user).

```sh
docker run -d --rm \
    -p 8080:8080 \
    -e NANOVM_API_TOKENS=dev-token \
    ghcr.io/ip888/nanovm-control-plane:latest

# Drive the lifecycle exactly like the source-build Quickstart below:
curl -s -X POST localhost:8080/v1/vms -H 'Authorization: Bearer dev-token' \
    -H 'content-type: application/json' -d '{}'
```

The image runs the **mock-backend** by default — same as the source
binary. A `--device /dev/kvm`-aware image with the real KVM backend
compiled in lands as a follow-up.

## Python client

A synchronous Python SDK for the control plane lives at
[`clients/python/`](clients/python/). Drives the same REST surface
the curl examples above hit, with dataclasses + typed exceptions:

```python
import nanovm

with nanovm.Client("http://localhost:8080", token="dev-token") as c:
    vm = c.create_vm()
    vm.start()
    result = vm.exec(program="python3", args=["-c", "print(1+1)"])
    print(result.stdout)      # "2\n" on a real KVM backend with a Python rootfs
    vm.destroy()
```

Install:

```sh
pip install nanovm
# or, for development:
pip install ./clients/python
```

PyPI releases ship via a Trusted-Publisher GitHub Actions workflow on
every `v*.*.*` tag — see
[`.github/workflows/python-publish.yml`](.github/workflows/python-publish.yml).

See [`clients/python/README.md`](clients/python/README.md) for the
full surface — snapshot/fork, cursor pagination, error handling,
and the `iter_vms()` helper that walks the whole result set.

## Quickstart — demo in 30 seconds (mock backend, no KVM)

One command, identical on Linux, macOS, and Windows. Only prerequisite
is a Rust toolchain (`rustup` from https://rustup.rs).

```sh
git clone https://github.com/ip888/Rust-nano-vm.git
cd Rust-nano-vm
cargo run -p control-plane --example demo --release
```

The example boots an in-process control plane backed by the
`MockHypervisor`, drives the full lifecycle, and prints a report:

```
✔ control-plane up on http://127.0.0.1:54231
✔ created   vm-0000000000000001
✔ started   vm-0000000000000001
✔ snapshot  snap-0000000000000001
✔ forked    vm-0000000000000002 in 0 ms
✔ forked    vm-0000000000000003 in 0 ms
✔ forked    vm-0000000000000004 in 0 ms
✔ forked    vm-0000000000000005 in 0 ms
✔ forked    vm-0000000000000006 in 0 ms

usage     : fork_count=5 fork_total_ms=0
metrics   : nanovm_forks_total{token="tok-demo-10"} 5
```

### Driving the REST API by hand

If you'd rather see the wire calls, run the binary and `curl` it. This
path is Linux/macOS only (uses POSIX job control and `until`) and needs
`jq` installed (`brew install jq` on macOS, `apt install jq` on
Debian/Ubuntu). Windows users: use the `cargo run --example demo` above
or run this in WSL.

```sh
cargo build --release -p control-plane

# Single-tenant: every token shares the implicit `default` org.
NANOVM_API_TOKENS=dev-token \
  ./target/release/nanovm-control-plane &

# Multi-tenant: bind each token to an explicit org. Tokens may not
# read or touch VMs/snapshots owned by a different org (the server
# returns 403 `cross_org` and list endpoints filter transparently).
#   NANOVM_API_TOKENS=acme:acme-tok,acme:acme-tok-2,globex:globex-tok

until curl -sf localhost:8080/healthz >/dev/null; do sleep 0.1; done

TOKEN="Authorization: Bearer dev-token"

VM=$(curl -s -X POST localhost:8080/v1/vms        -H "$TOKEN" -H 'content-type: application/json' -d '{}' | jq -r .id)
curl -s -X POST localhost:8080/v1/vms/$VM/start    -H "$TOKEN" >/dev/null
SNAP=$(curl -s -X POST localhost:8080/v1/vms/$VM/snapshot -H "$TOKEN" | jq -r .id)

for i in 1 2 3 4 5; do
  curl -s -X POST localhost:8080/v1/snapshots/$SNAP/fork -H "$TOKEN" \
    | jq -c '{vm: .vm.id, fork_ms, fork_count}'
done

curl -s localhost:8080/v1/usage -H "$TOKEN" | jq
curl -s localhost:8080/metrics | head -20

# Stop the backgrounded server when done. If you don't, the next run
# will fail with "Address already in use".
kill %1
```

### Sandbox-action API (one-shot fork-exec-destroy)

For AI-agent tool use, the VM-lifecycle API is unwieldy — agents
just want "run this in a sandbox and give me back the output."
`POST /v1/sandbox/invoke` does exactly that: forks a fresh VM from
a snapshot, runs an action, destroys the VM, returns a flat
envelope. Five actions: `execute_python`, `execute_shell`,
`read_file`, `write_file`, `list_files`.

```sh
SNAP=...   # snapshot id captured above

curl -s -X POST localhost:8080/v1/sandbox/invoke \
  -H "$TOKEN" -H 'content-type: application/json' \
  -d '{
    "snapshot": '"$SNAP"',
    "action": "execute_shell",
    "command": "echo hello from sandbox"
  }' | jq
# { "stdout": "hello from sandbox\n", "stderr": "", "exit_code": 0,
#   "duration_ms": 14, "cold_start": true }
```

`cold_start: false` indicates the VM came off the warm pool
(sub-millisecond fork); `cold_start: true` means a fresh restore.
Set `NANOVM_SANDBOX_SNAPSHOT_ID=<id>` on the server to make the
`snapshot` body field optional.

## Quickstart — real KVM, real numbers

Linux host with `/dev/kvm`:

```sh
# Build kernel + initramfs once (see docs/kvm-host.md):
tools/kernel/build-tiny-kernel.sh
tools/initramfs/build-initramfs.sh

# Boot one guest, snapshot it, fork 100 children, measure:
cargo run -p bench --features kvm --release --bin nanovm-fork-bench -- \
  --forks 100 --alive 50 --settle-secs 2
```

Expected output on a modest laptop:

```
fork latency  : p50 12.1 ms  p95 14.7 ms  p99 16.2 ms
density       : N=50, host Pss/fork 0.51 MiB, shared 91.4%
projection    : ~30000 concurrent forks per 16 GiB host
```

## Architecture

```
 ┌─────────────────────────────────────────────────────┐
 │  nanovm CLI  /  control-plane (axum REST)           │
 │  bearer-auth · per-token quota · /metrics           │
 └───────────────┬─────────────────────────────────────┘
                 │ agent-sandbox-proto (serde / JSON)
                 ▼
 ┌─────────────────────────────────────────────────────┐
 │  vm-core :: trait Hypervisor                        │
 ├─────────────────────────────────────────────────────┤
 │  vm-kvm (KVM, snapshot/fork)  │  vm-mock (CI tests) │
 └────────┬──────────────────────┴─────────────────────┘
          │
          ▼
 ┌─────────────────────────────────────────────────────┐
 │  virtio-vsock     │  snapshot (MAP_PRIVATE CoW)     │
 │  custom Rust impl │  manifest + RAM backing file    │
 └─────────────────────────────────────────────────────┘
          │
          ▼
 ┌─────────────────────────────────────────────────────┐
 │  guest-agent (static musl, ~150 KiB)                │
 │  exec, stdio streaming, signal handling             │
 └─────────────────────────────────────────────────────┘
```

Full narrative in [`docs/architecture.md`](docs/architecture.md).
Head-to-head against E2B, Firecracker, Kata, gVisor in
[`docs/comparison.md`](docs/comparison.md).

## Workspace layout

```
crates/
  vm-core/        Hypervisor trait, VmConfig, VmHandle, VmError
  vm-mock/        In-memory backend, no KVM required (used by CI)
  vm-kvm/         KVM backend with snapshot/restore + MAP_PRIVATE fork
  virtio-vsock/   Host↔guest vsock transport (custom Rust)
  virtio-queue/   Shared virtio split-ring code
  virtio-fs/      Host↔guest filesystem (in progress)
  snapshot/       Snapshot manifest + backing-file format
  guest-agent/    Static musl binary running inside the guest
  control-plane/  axum REST API: auth, quota, metering, /metrics
  proto/          Shared agent-sandbox-proto types
  cli/            `nanovm` binary
  bench/          nanovm-fork-bench: latency + Pss/density
```

## Status

| Component | State |
| --- | --- |
| Workspace, `Hypervisor` trait, mock backend, CI | ✅ |
| `vm-kvm` boots minimal kernel; serial output end-to-end | ✅ |
| virtio-vsock + musl guest agent, `nanovm exec` round-trip | ✅ |
| Snapshot + fork; ~12 ms p50 cold start, measured | ✅ |
| Control plane REST: auth, quota, metering, Prometheus | ✅ |
| Warm pool: pre-restored forks for sub-ms hand-out (opt-in) | ✅ |
| Streaming exec via Server-Sent Events (`POST /v1/vms/:id/exec/stream`) on both mock and real KVM | ✅ |
| Python guest rootfs (Alpine 3.20 + Python 3.12); `python3 -c "print(1+1)"` round-trip on real KVM | ✅ |
| Host↔guest file push/pull via `/v1/vms/:id/files` (vsock RPC, real KVM end-to-end) | ✅ |
| Seccomp-BPF sandbox on the VMM process (opt-in via `NANOVM_SECCOMP=1`) | ✅ |
| Cgroups v2 process-wide memory + CPU caps on the VMM (opt-in via `NANOVM_VMM_MEMORY_LIMIT_MIB` / `NANOVM_VMM_CPU_QUOTA_PCT`) | ✅ |
| Durable snapshot storage on S3 / MinIO / R2 (`NANOVM_SNAPSHOT_STORE=s3://...`, opt-in via `--features s3`) | ✅ |
| Python SDK (`pip install ./clients/python`) — synchronous, typed exceptions | ✅ |
| Docker image on GHCR (`ghcr.io/ip888/nanovm-control-plane`) | ✅ |
| virtio-fs `mount` from inside the guest (FUSE wire types + dispatch are done; KVM device wiring is the gap) | in progress |

Pre-1.0. Full roadmap in [`docs/PLAN.md`](docs/PLAN.md).

## Use cases this is built for

- **AI agent eval pipelines.** Fan out 1000 variants of a base image to
  run a benchmark in parallel; throw them away in milliseconds. Set
  `NANOVM_WARM_POOL_PER_SNAPSHOT=N` to hide the cold-restore cost
  behind a background queue. See
  [`docs/blog/04-12ms-eval-fanout.md`](docs/blog/04-12ms-eval-fanout.md)
  for the worked numbers vs containers.
- **Self-hosted code interpreters.** Drop-in OSS alternative to E2B for
  teams that need on-prem (healthcare, finance, defense, EU data
  residency). See
  [`docs/blog/03-regulated-ai-sandboxes.md`](docs/blog/03-regulated-ai-sandboxes.md)
  for the audit-narrative angle.
- **CI for untrusted PRs.** Stronger isolation than a container, faster
  than a fresh VM, with a REST API your runner can drive.
- **Per-user sandboxes for AI products.** One snapshot per language
  toolchain, forked per request.

## Built with AI assistance

This project is developed by one engineer with substantial
pair-programming help from Claude (Anthropic) and GitHub Copilot. The
agentic workflow is visible in `git log` — many line-level changes were
drafted by an AI agent, then reviewed, tested, integrated, and committed
by me.

Architecture decisions, the choice of the snapshot-fork primitive as
the wedge, the wire format, the API surface, the performance targets,
and what ships when are mine. The code is the artifact — to evaluate
the project, read
[`crates/vm-kvm/src/vmstate.rs`](crates/vm-kvm/src/vmstate.rs)
(`MAP_PRIVATE` fork-many, snapshot/restore),
[`crates/snapshot/`](crates/snapshot/) (on-disk format, userfaultfd CoW
fault handler), and [`crates/bench/`](crates/bench/) (the headline
numbers above are reproducible on any KVM host).

## Contributing

Pre-1.0; expect churn. File an issue before sending a large PR. The
trait boundaries in [`docs/architecture.md`](docs/architecture.md) are
load-bearing — keep all KVM code behind `vm-kvm` and test against
`vm-mock`.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option. Contributions are accepted under the same dual license.
