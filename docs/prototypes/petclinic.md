# Petclinic prototype (Milestone 1)

The canonical demo target for the enterprise-Java product line:
snapshot a warmed Spring Petclinic inside a nanovm KVM guest, fork it
many times, measure per-fork time to first HTTP-200 on `/vets`.

The **single-jar Petclinic** variant is scoped for this milestone.
The seven-service Spring-Petclinic-Microservices variant lands as
Milestone 2 (separate PR series).

## Milestone-1 landing plan

Three PRs on `main`, each self-contained:

| PR | Scope | Depends on |
|---|---|---|
| **#267 (merged)** | `tools/java-rootfs/` scaffold: Dockerfile, fetch.sh, guest init/warmup, initial docs | — |
| **#268 (this PR)** | `build.sh` now emits `initramfs.cpio.gz` in addition to `rootfs.ext4`; docs updated to reflect the initramfs-based boot path (see rationale below) | #267 |
| **#269 (planned)** | New `crates/bench` binary `nanovm-jvm-bench`: boot → warmup → snapshot (cold\|warm\|both) → fork N → HTTP-200 p50/p90/p99 | #267 + #268 |

## Boot path: initramfs, not virtio-blk

The Milestone 1 demo uses the **initramfs boot path**, not a virtio-blk
disk device. Two reasons:

1. **vm-kvm already supports initramfs.** `VmConfig.initrd` is fully
   plumbed today: `load_initrd()` copies the archive high in guest
   RAM, boot params get `ramdisk_image` / `ramdisk_size` set, kernel
   unpacks into a tmpfs and execs `/sbin/init`. No new device code.
2. **MAP_PRIVATE fork of guest RAM captures the whole rootfs for
   free.** The unpacked initramfs lives in guest memory as tmpfs; our
   fork mechanism does copy-on-write on that memory, so each forked
   child inherits an identical, private rootfs at zero copy cost.
   That's exactly the shape the sub-second fork-many demo wants.

Virtio-blk stays on the roadmap for a production-shape disk-backed
rootfs (larger images, stateful workloads). It's a substantial
addition to `crates/vm-kvm` and doesn't unblock the demo, so it's
deferred to after the M1 demo lands.

`build.sh` still emits `rootfs.ext4` alongside `initramfs.cpio.gz`
so a future virtio-blk path can reuse the same content without a
rebuild.

## What the demo shows

Two side-by-side numbers on the same rootfs:

| Snapshot taken | Restore path | Expected `time-to-first-HTTP-200` p50 |
|---|---|---|
| **Cold** — right after JVM boot, before Spring context finishes loading | Spring finishes init in the forked child | ~500 ms |
| **Warm** — after the in-guest warmup driver hits `/actuator/health` + duration-based JIT warmup | First request served from restored state | **~150 ms** |

The cold number proves that KVM-level snapshot preserves an
in-progress JVM heap correctly. The warm number is the marketing
result. Both are recorded so anyone reproducing the demo can compare.

## Reproduce end-to-end

### 1. Prerequisites

- Linux host with `/dev/kvm` (needed for step 4 only; steps 2–3 work
  on any Docker-capable machine including Mac M1, see
  "Developer platform matrix" below)
- Docker with `buildx`
- Optional: `e2fsprogs` + `sudo` for the ext4 pack (`SKIP_EXT4=1`
  skips it)
- ~1 GiB free in `tools/java-rootfs/cache/`
- Rust toolchain matching `rust-toolchain.toml`

### 2. Fetch the guest kernel

```sh
tools/kvm-images/fetch.sh tools/kvm-images/cache
```

The nanovm-jvm-bench step below reads `tools/kvm-images/cache/vmlinux`;
a fresh checkout has no file there.

### 3. Build the rootfs

```sh
tools/java-rootfs/build.sh
# or, on a rootless host / CI:
SKIP_EXT4=1 tools/java-rootfs/build.sh
```

Host prerequisites (checked upfront by `build.sh`):
- `docker` with `buildx`
- `cpio`, `gzip`, `find` — the initramfs pack step needs them; on
  macOS they ship with the base OS (BSD cpio; short-option cpio calls
  in `build.sh` are portable across GNU and BSD variants).
- `sudo` + `mkfs.ext4` from `e2fsprogs` — only if you pack the ext4
  artifact (`SKIP_EXT4=1` skips it entirely).

Outputs:
- `tools/java-rootfs/cache/initramfs.cpio.gz` — the demo boot path
- `tools/java-rootfs/cache/rootfs.ext4` — unused today, kept for the
  future virtio-blk story

### 4. Smoke-check the rootfs contents (no KVM required)

```sh
docker run --rm -it --privileged --network host nanovm-java-rootfs:local
```

`--privileged` is required because `init.sh` mounts `/proc`, `/sys`,
`/dev` (devtmpfs), `/tmp` and `/run` — an unprivileged container
lacks `CAP_SYS_ADMIN` for those. Under the nanovm KVM guest the same
init runs unprivileged (guest kernel gives PID 1 the caps); the init
script tolerates already-mounted or unmountable pseudo-fs so both
paths produce the same behavior.

Expected on stdout within ~10 s:

```
[init] launching JVM: java -Xmx1g -Xshare:auto -XX:+UseZGC ... -jar /opt/petclinic.jar
[init] JVM pid=42
[warmup] waiting up to 60s for http://127.0.0.1:8080/actuator/health
[warmup] health 200 after 6s
[warmup] jit warmup: 30s across http://127.0.0.1:8080/vets ...
[warmup] jit warmup done: 1240 ok, 0 failed
NANOVM_PETCLINIC_READY
```

If Petclinic is already listening on 8080 on your host, either kill
that process or drop `--network host` and add `-p 8080:8080` — the
warmup driver hits `127.0.0.1:8080` from *inside* the container.

### 5. Boot under KVM and run the snapshot-fork benchmark

*(Available after `nanovm-jvm-bench` lands in PR #269.)*

```sh
cargo run --release --features kvm -p bench --bin nanovm-jvm-bench -- \
    --kernel    tools/kvm-images/cache/vmlinux \
    --initramfs tools/java-rootfs/cache/initramfs.cpio.gz \
    --memory-mib 2048 \
    --snapshot-at both \
    --forks 20 --warmup 5
```

The `--memory-mib 2048` is intentional: the initramfs unpacks to
~450 MiB in guest RAM; the JVM wants 1 GiB heap; the kernel + slack
eat the rest. Emits a two-column markdown table with p50 / p90 / p99
for both the cold- and warm-snapshot paths, plus a histogram.

## Developer platform matrix

Not every step needs a KVM Linux host. The tests are stratified:

| Test level | Runs on | What it verifies |
|---|---|---|
| **L1 — mock backend** (`cargo test --workspace`) | any OS/CPU (Linux, macOS Intel + Apple Silicon, Windows via WSL) | Rust API surface, protocol framing, snapshot format, ownership store, the whole non-hardware code path |
| **L2 — rootfs smoke** (`docker run`) | any Docker-capable host, including Mac M1 (auto-emulates x86_64 via QEMU inside Docker Desktop; slower but works) | The rootfs boots, JVM launches, Spring Petclinic hits ready marker |
| **L3 — KVM boot + snapshot + fork** (`nanovm-jvm-bench`) | Linux + `/dev/kvm` + x86_64 (Intel VT-x or AMD-V) | The actual demo numbers |

Contributor workflow on Mac M1:
- **L1 during coding** — `cargo test --workspace` runs full mock-backend suite in <10 s on M1.
- **L2 before PR** — `tools/java-rootfs/build.sh && docker run --privileged` on M1 exercises the whole rootfs path minus KVM. `build.sh` sets `TARGET_PLATFORM=linux/amd64` by default so Docker Desktop selects the amd64 base images and emulates the runtime under Rosetta/QEMU — ~3–5× slower than native x86 but confirms the boot sequence, JVM start, Spring context, warmup driver. Explicit `--platform` is required because Oracle JDK 21 ships x86-64 binaries; without the pin, buildx would pull an arm64 base on M1 and `java -Xshare:dump` in the JDK-extract stage would fail with exec-format error.
- **L3 for demo numbers** — needs a Linux/KVM host. Options: EC2 metal (`i3.metal`, `m5.metal`), GCP with nested-virt, a Linux workstation, or the CI matrix once we add a KVM-capable runner.

We're not building an in-browser demo path in Milestone 1 (would need
a hosted control-plane deployment on a metal instance behind a public
API + a web UI to drive it — that's a separate marketing milestone).

## Pinning workflow

The three digest pins land together with the release-track PR that
follows Milestone 1. Not part of the scaffold PRs.

Before any release-track branch merges:
1. **Oracle JDK 21.0.5 tarball SHA-256** — `tools/java-rootfs/fetch.sh`
   currently `SKIP`; refuses to run under `NANOVM_STRICT_PINS=1`.
2. **Spring Petclinic commit hash** — `PLACEHOLDER_PIN_BEFORE_MERGE`
   in `Dockerfile`'s `PETCLINIC_REF` build arg.
3. **`debian:12-slim` digest** — `Dockerfile`'s `DEBIAN_DIGEST` arg,
   applied to both `FROM` lines.

## What Milestone 1 explicitly does NOT include

- **The seven-service Petclinic-microservices variant** — Milestone 2.
- **Cross-guest networking / static hostname map** — Milestone 2
  (Eureka needs stable IP-to-hostname resolution across forks).
- **CRaC-based warmup coordination** — this milestone uses console-tail
  for the ready marker, which is simpler and covers the whole demo.
  Vsock signaling replaces it in a later milestone if profiling shows
  console-read latency matters.
- **ARM64 (aarch64) support** — this milestone targets x86_64 only,
  matching the enterprise-Java v1 platform scope in `CLAUDE.md`.
- **Virtio-blk device in vm-kvm** — deferred (see "Boot path" above).
- **In-browser hosted demo** — separate marketing milestone.
