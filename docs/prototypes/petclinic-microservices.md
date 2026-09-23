# Petclinic Microservices prototype (Milestone 2)

The seven-service variant of Petclinic — Spring Cloud Config + Netflix
Eureka + Spring Boot Admin + Spring Cloud Gateway + three business
services (customers / vets / visits). Same fork-many demo as
Milestone 1, but the snapshot captures the **entire enterprise stack
in one guest**, not just one Spring Boot app.

## Milestone-2 landing plan

Three PRs on `main`, each self-contained. This document lives with
PR #1 and is updated as follow-ups land.

| PR | Scope | Status |
|---|---|---|
| **M2 PR #1** | `tools/java-microservices-rootfs/`: Dockerfile that builds all seven services + orderly startup + Eureka-readiness gate + docs | This PR |
| **M2 PR #2** | Add virtio-net bridge to `crates/vm-kvm` so `NANOVM_TIER=infra` and `NANOVM_TIER=app` can run in separate guests over L2. Tier-split demo. Deferred until PR #1's single-guest demo is validated | Planned |
| **M2 PR #3** | `nanovm-microservices-bench`: snapshot the warm stack, fork N times, measure per-fork time to end-to-end HTTP 200 through api-gateway | Planned |

## What the demo shows

Same shape as M1, larger stack:

| Snapshot taken | Restore path | Expected p50 |
|---|---|---|
| **Cold** — right after api-gateway TCP socket opens | Each fork completes Eureka registration + route init | ~3–5 s |
| **Warm** — after all seven services register with Eureka and `/api/customer/owners` returns 200 | First request served from restored state | **~200–400 ms** |

Cold-boot baseline for reference — booting the same stack without
snapshots takes 90–120 s on a mid-tier host. The warm-snapshot fork
number therefore represents a ~300–500× speedup on **enterprise
microservices cold-start**.

## Reproduce end-to-end

### 1. Prerequisites

- Linux host with `/dev/kvm` (only step 5 needs it; earlier steps
  work on any Docker-capable host including Mac M1)
- Docker with `buildx`
- `cpio`, `gzip`, `find` (checked upfront by `build.sh`)
- Optional: `e2fsprogs` + `sudo` for the ext4 pack (`SKIP_EXT4=1` skips)
- ~2 GiB free in `tools/java-microservices-rootfs/cache/`
- Rust toolchain matching `rust-toolchain.toml`

### 2. Fetch the guest kernel

```sh
tools/kvm-images/fetch.sh tools/kvm-images/cache
```

### 3. Build the microservices rootfs

```sh
tools/java-microservices-rootfs/build.sh
# rootless / CI (initramfs only, no ext4):
SKIP_EXT4=1 tools/java-microservices-rootfs/build.sh
```

Outputs:
- `tools/java-microservices-rootfs/cache/initramfs.cpio.gz` — the demo boot path
- `tools/java-microservices-rootfs/cache/rootfs.ext4` — future virtio-blk path

**Build time expectation:** the Maven build in stage 1 compiles all
seven services. First run pulls ~600 MiB of Maven deps and takes
10–15 minutes on a laptop; incremental rebuilds hit the Maven cache
and finish in 2–3 minutes. `docker buildx` layer cache makes stage 2
+ 3 near-instant after the first run.

### 4. Smoke-check the rootfs contents

```sh
docker run --rm -it --privileged --network host nanovm-java-microservices-rootfs:local
```

Expected sequence on stdout within ~2 minutes:

```
[init] handing off to tier-launcher
[tier-launcher] NANOVM_TIER=all
[tier-launcher] starting config-server (Xmx512m --server.port=8888)
[tier-launcher] waiting up to 90s for config-server health ...
[tier-launcher] config-server healthy after 22s
[tier-launcher] starting discovery-server ...
[tier-launcher] discovery-server healthy after 18s
[tier-launcher] starting admin-server ...
[tier-launcher] admin-server healthy after 14s
[tier-launcher] starting api-gateway ...
[tier-launcher] starting customers-service ...
[tier-launcher] starting vets-service ...
[tier-launcher] starting visits-service ...
[warmup] waiting up to 90s for api-gateway TCP :8080
[warmup] api-gateway TCP socket open after 25s
NANOVM_MICROSERVICES_COLD
[warmup] waiting up to 180s for full-stack readiness
[warmup] end-to-end 200 from customers-service after 14s
[warmup] jit warmup: 45s across gateway routes
[warmup] jit warmup done: 1800 ok, 0 failed
NANOVM_MICROSERVICES_READY
```

If `/api/customer/owners` doesn't reach 200 within `READY_WAIT_SECS`,
the warmup dumps `tail -n 40` of each service's log to stderr — the
usual culprit is a config-server template pointing at a hostname
that doesn't resolve inside the container.

### 5. Boot under KVM and run the microservices bench

*(Available after M2 PR #3 lands. Command shape:)*

```sh
cargo run --release --features kvm -p bench --bin nanovm-microservices-bench -- \
    --kernel    tools/kvm-images/cache/vmlinux \
    --initramfs tools/java-microservices-rootfs/cache/initramfs.cpio.gz \
    --memory-mib 4096 \
    --snapshot-at both \
    --forks 10 --warmup 2
```

`--memory-mib 4096` covers the 900 MiB unpacked rootfs + seven JVM
heaps + kernel + slack. Reports per-mode p50/p95/p99 latencies from
`restore()` (for warm) or `restore()` + wait-for-`NANOVM_MICROSERVICES_READY`
(for cold), plus the side-by-side cold-vs-warm speedup table.

## Design choices

- **All seven services in one guest** (for now). Simpler than
  tier-split, avoids adding virtio-net + host bridge to `vm-kvm` for
  the first demo. The `NANOVM_TIER=infra|app` cmdline knob is wired
  in `tier-launcher.sh` today but only `all` (default) is exercised
  until M2 PR #2 lands virtio-net bridge support.
- **Orderly startup enforced host-side by the guest launcher**. The
  Spring Cloud clients don't retry infinitely on a missing Config
  Server — we don't want to time-out-and-error our way through a
  race that a `wait_health` gate resolves cleanly.
- **`-Xmx256m` per non-infra JVM, `-Xmx512m` for config-server,
  `-Xmx384m` for Eureka.** Total heap ~2 GiB. Config server holds
  every service's config in memory so it gets the biggest slice;
  Eureka's per-instance registry entries add up but not enough to
  need > 384 MiB in this demo.
- **Eureka readiness proved by end-to-end 200 through api-gateway.**
  Faster and more honest than counting registered-apps in Eureka's
  REST API — a 200 from `/api/customer/owners` means api-gateway can
  look up customers-service in Eureka AND customers-service can
  serve JPA/H2-backed queries. Anything less is a partial ready.

## Not in scope for Milestone 2

- **CRaC coordination.** Same as M1 — vsock signalling replaces
  console-tail if profiling shows a win, in a later milestone.
- **Distributed tracing / observability.** Spring Sleuth / Zipkin
  are wired inside Petclinic upstream but we don't reproduce their
  collector here — the demo focuses on cold-start numbers, not on
  showing a distributed-trace UI.
- **Persistent H2 across forks.** Every fork inherits the parent's
  H2 database state — which is fine for the demo but means each
  fork sees the same seed data. A production shape would want
  per-fork H2 files or a stateful DB reachable over the network.
- **ARM64.** Same posture as M1 — x86_64 first, arm64 as a v2 goal.
