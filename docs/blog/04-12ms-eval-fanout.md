# 12 ms fan-out for AI-agent eval pipelines: when MAP_PRIVATE beats containerd

> **TL;DR.** Agent evals fan out a base image across N sandboxes,
> run for seconds, throw them away. Containers spend most of that
> wall-time on cgroup setup, image pull, and OverlayFS housekeeping.
> A microVM with snapshot+fork as a first-class primitive collapses
> the per-sandbox cost to a `mmap(MAP_PRIVATE)`. Measured: **~12 ms p50
> cold start, ~0.5 MiB Pss per fork at N=50, ~30 000 concurrent
> sandboxes projected per 16 GiB host.**

## The shape of the workload

Pick your favourite agent-eval benchmark:

- **SWE-bench Lite** — 300 GitHub issues, each in its own repo
  checkout, each needing `pip install`, run the agent, run the tests.
- **HumanEval-style** — 164 Python tasks, run completions for each, run
  the unit tests for each completion.
- **An in-house regression suite** — every PR runs the agent against
  every benchmark problem; if anything regresses you get paged.

All of these have the same pattern: **one prepared base image
(language toolchain, deps, golden state) → many short-lived
sandboxes (one per task) → throw away.** The agent's wall-time is
the wall-time of `N × per-task work`. The sandbox boot cost should
disappear into the noise. With containers it doesn't.

## Where the wall-time actually goes with containers

A *fresh* container boot looks fast on a microbenchmark — Docker's
own numbers cite "less than a second". In a fan-out pipeline you
care about three different costs the microbenchmarks hide:

1. **Image-layer setup.** OverlayFS has to mount the union of N
   layers per container. With a heavy base image (Python + uv + node
   + pulled deps) each container fork is touching dozens of mounts.
   Disk I/O bound on cold cache; CPU bound on hot.
2. **Page-cache duplication.** Even when the underlying image data
   is shared, each container gets its own dirty pages for whatever
   the toolchain reads. Run 1000 Python containers and `ps -ax` shows
   the same byte sequence reloaded 1000 times.
3. **cgroup + namespace teardown.** Containers feel cheap to start
   and end up paying the cost on stop: cgroup destruction, namespace
   tear-down, OverlayFS unmount. Visible in `iostat` and `kworker`
   CPU during eval runs.

You can paper over each of these — overlay2 with a hot page cache,
prewarm pools, leaky reuse — and end up with a system whose
behaviour during evals is a mystery. The honest cost is **~50–200 ms
per container** under realistic fan-out, with memory growing linearly
in N because the kernel can't deduplicate dirty pages.

## What microVMs do differently here

The microVM playbook says *boot one VM to the warm state, capture
its memory + vCPU state, and "fork" subsequent runs from that
capture*. Two execution models exist:

- **Snapshot/restore**: copy the snapshot RAM into a fresh anonymous
  mapping per VM. Firecracker does this in ~125 ms. The cost is
  proportional to the snapshot size — for a 200 MiB warm Python
  process that's 200 MiB of `memcpy` per fork.
- **MAP_PRIVATE CoW fork**: `mmap(MAP_PRIVATE, fd, …)` the snapshot
  RAM file. The fork pays for nothing it doesn't touch. The kernel
  serves the read-only golden pages from the page cache to every
  child. **No memcpy.** ([Why this works, in detail.](01-mmap-private.md))

`rust-nano-vm` builds the second model as the primitive. Cold start
is a `mmap` and a `KVM_SET_USER_MEMORY_REGION` away — measured at
**~12 ms p50** on a stock i5 laptop. Per-fork Pss (proportional set
size, the honest memory number) **decreases as fan-out grows**
because the marginal cost of fork #50 is lower than fork #10 — more
sharers amortise the same shared pages.

## The math, made concrete

A SWE-bench-style harness running 1000 tasks against one Python
base image, on a 16 GiB host. Assume the warm base image is ~7 MiB
of code + data (modest; production warm images are 100s of MiB,
which the CoW story handles even better).

**Containers (optimistic):**

```
per-task boot   :    80 ms   (overlay + namespace + cgroup, cold cache amortised)
per-task RAM    :   ~20 MiB  (libpython + your script, no page sharing)
concurrent cap  :  16 GiB / 20 MiB  =  ~800 sandboxes
total wall-time : 1000 × 80 ms      = ~80 seconds before any work runs
```

**rust-nano-vm:**

```
per-task fork   :    12 ms   (mmap + KVM_RUN)
per-task Pss    :  ~0.5 MiB  (shared via MAP_PRIVATE CoW)
concurrent cap  :  16 GiB / 0.5 MiB = ~30 000 sandboxes
total wall-time : 1000 × 12 ms      = ~12 seconds before any work runs
```

The difference per run is ~7× wall-clock on the boot phase and ~40×
on memory ceiling. For one eval run it's an hour saved. For a
nightly job it compounds.

## What `rust-nano-vm` gives you for this shape of workload

Four properties that matter specifically for eval-pipeline use:

### 1. Fork is the headline primitive

The REST API has `POST /v1/snapshots/{id}/fork`. You don't restore;
you fork. Wire-level:

```
$ curl -s -X POST localhost:8080/v1/snapshots/1/fork -H "Authorization: Bearer t"
{"vm":{"id":2,"display":"vm-...","state":"created"},
 "fork_ms":12,"fork_count":3,"fork_total_ms":36}
```

`fork_ms` is what you'd log into your run metadata; `fork_count` and
`fork_total_ms` are server-side metering so you can attribute compute
back to the agent/run.

### 2. Per-token quota on the expensive route

`/fork` is the expensive endpoint (it spawns work). A misbehaving
agent that goes runaway and tries to fork 10 000 times hits a
per-token token-bucket quota (`NANOVM_FORK_RPS` /
`NANOVM_FORK_BURST`) and gets 429 + `Retry-After`. Your other agents
on other tokens keep working.

### 3. Honest accounting

The bench harness reports Pss from `/proc/self/smaps_rollup`, not
RSS. RSS double-counts shared pages and **overstates fork cost by
5–10×** — exactly the failure mode a "we use containers, memory's
fine" team eventually hits in production.

If your eval pipeline currently runs with RSS-based memory ceilings,
moving to Pss will surprise you in a good way.

### 4. The control plane is one binary

`nanovm-control-plane` is a single static binary. No jailer process,
no Docker daemon, no orchestrator. Drop it on your eval-runner box
behind whatever auth/proxy you already have. ~330 lines of axum on
the network surface — auditable in an afternoon.

## A worked example: SWE-bench-style fan-out

Conceptual driver (mock backend, no KVM needed to verify the shape):

```sh
cargo run -p control-plane --example demo --release
```

Or the bench harness against the real KVM backend on a Linux host
with `/dev/kvm`:

```sh
cargo run -p bench --features kvm --release --bin nanovm-fork-bench -- \
    --count 1000 --alive 50 --settle-secs 2
```

Expected shape of the output (recorded on an i5 laptop):

```
fork latency  : p50 12.1 ms  p95 14.7 ms  p99 16.2 ms
density       : N=50, host Pss/fork 0.51 MiB, shared 91.4%
projection    : ~30 000 concurrent forks per 16 GiB host
```

`--alive` is how many forks the harness keeps live concurrently;
`--count` is the total. The N=50 row above means: when 50 children
share the same MAP_PRIVATE base, each one's *proportional* memory
share is 0.51 MiB, ~91% of pages are still being served from the
shared file.

In a real harness you'd hand each fork its run id, exec the task,
collect stdout, destroy. The lifecycle is:

```
POST /v1/snapshots/{base}/fork  →  { vm: { id: N } }
POST /v1/vms/{N}/exec  → { stdout: ..., stderr: ..., exit: 0 }
DELETE /v1/vms/{N}
```

## When this approach loses

The MAP_PRIVATE CoW story is honest about its caveats:

- **Write-heavy first phase.** If every fork's first 200 ms is
  rewriting a 100 MiB heap, you pay for all of it. Pss converges
  toward RSS for that fork. For an agent eval where the work is
  usually "read a few files, run pytest", this doesn't bite.
- **Snapshot capture has its own cost.** Booting the base image to a
  warm state and capturing it takes the full ~125 ms (or whatever
  your boot time is). You amortise that across every fork from it,
  which is the right shape — but you do need to do the capture step.
- **Same-kernel, same-CPU.** The snapshot encodes vCPU state. You
  can't capture on x86_64 KVM 6.6 and restore on aarch64 KVM 6.1.
  For an in-cluster eval pipeline this is fine; for cross-region
  fan-out it isn't yet.
- **You still need a guest agent.** The microVM gives you isolation;
  it doesn't give you "run this command and stream stdout". The
  guest-side `nanovm-agent` (static musl, baked into the initramfs)
  is what makes `POST /v1/vms/{N}/exec` work end-to-end.

For an eval pipeline running on one host, against one snapshot, for
seconds of work — the use case the project was built for — none of
these caveats apply.

## Try it

Two evaluation paths:

**Source build** (any platform with a Rust toolchain — mock backend,
no KVM required to see the API shape):

```sh
git clone https://github.com/ip888/Rust-nano-vm
cd Rust-nano-vm
cargo run -p control-plane --example demo --release
```

**Real numbers** (Linux + `/dev/kvm`):

```sh
cargo run -p bench --features kvm --release --bin nanovm-fork-bench -- \
    --count 100 --alive 50 --settle-secs 2
```

**Prebuilt binaries** are published for `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu` (Graviton / Ampere / Oracle A1), and
`aarch64-apple-darwin` (Apple Silicon dev) on the
[releases page](https://github.com/ip888/Rust-nano-vm/releases/latest)
with sidecar SHA256s for verification.

## What's interesting if you take this seriously

If you're running an agent-eval harness and the boot phase shows up
in your wall-clock budget — file an issue with the rough shape of
your workload. The interface (snapshot+fork as the unit of
execution) is intentionally narrow because that's what the projects
that win are: focused on one motion and doing it well.

The repo lives at https://github.com/ip888/Rust-nano-vm.

---

Companion posts:

- [How rust-nano-vm cold-starts in ~12 ms: `mmap(MAP_PRIVATE)` is the whole trick](01-mmap-private.md)
- [Faithful KVM snapshot/restore in <1000 lines of Rust](02-snapshot-restore.md)
- [Running AI agents in regulated environments](03-regulated-ai-sandboxes.md)
