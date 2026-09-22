# Petclinic prototype (Milestone 1)

The canonical demo target for the enterprise-Java product line:
snapshot a warmed Spring Petclinic inside a nanovm KVM guest, fork it
many times, measure per-fork time to first HTTP-200 on `/vets`.

The **single-jar Petclinic** variant is scoped for this milestone.
The seven-service Spring-Petclinic-Microservices variant lands as
Milestone 2 (separate PR).

## Milestone-1 pieces and their landing order

The milestone splits into three commits on the same branch. The first
one (this PR at scaffold time) lands the rootfs artifact half; the
next two land the vm-kvm plumbing and the host driver.

| Commit | What lands | Blocks |
|---|---|---|
| **1. `feat(petclinic-m1): scaffold Java rootfs builder`** (this PR) | `tools/java-rootfs/` builder + Dockerfile + guest init + warmup.sh + docs | — |
| **2. `feat(vm-kvm): virtio-blk rootfs attachment`** (this PR, follow-up commit) | `crates/vm-kvm` consumes `VmConfig.rootfs`, attaches it at `/dev/vda`, sets `root=/dev/vda init=/sbin/init` on cmdline | Real KVM boot of this rootfs |
| **3. `feat(bench): nanovm-jvm-bench`** (this PR, follow-up commit) | Host binary that boots, warms, snapshots (`--snapshot-at cold\|warm\|both`), forks N times, reports p50/p90/p99 | The demo run in step 4 below |

All three land on this branch before the PR merges. Reviewing the
rootfs half first keeps each commit's diff bounded.

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

- Linux host with `/dev/kvm`
- Docker with `buildx`
- `e2fsprogs` (for `mkfs.ext4`), `sudo` (for the loopback mount in
  `build.sh`)
- ~1 GiB free in `tools/java-rootfs/cache/`
- Rust toolchain matching `rust-toolchain.toml`

### 2. Build the rootfs

```sh
tools/java-rootfs/build.sh
```

Output: `tools/java-rootfs/cache/rootfs.ext4` (≤ 450 MiB uncompressed).

### 3. Smoke-check the rootfs contents (no KVM required)

The vm-kvm virtio-blk attachment is a follow-up commit, so you can't
yet boot this through the repo's KVM backend directly. But the same
Docker image the rootfs is built from can be `docker run`'d as a
sanity check that the JVM starts, Spring boots, and the warmup driver
emits the ready marker:

```sh
docker run --rm -it --network host nanovm-java-rootfs:local
```

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

### 4. Boot under KVM and run the snapshot-fork benchmark

*(Available after the vm-kvm virtio-blk and `nanovm-jvm-bench`
follow-up commits land in this PR.)*

```sh
cargo run --release --features kvm -p bench --bin nanovm-jvm-bench -- \
    --kernel  tools/java-rootfs/cache/vmlinux \
    --rootfs  tools/java-rootfs/cache/rootfs.ext4 \
    --snapshot-at both \
    --forks 20 --warmup 5
```

Emits a two-column markdown table with p50 / p90 / p99 for both the
cold- and warm-snapshot paths, plus a histogram. The `vmlinux` reuses
the Firecracker sample kernel from `tools/kvm-images/`.

## Pinning workflow

Three moving pieces need real values before this PR merges to `main`.
All three are called out in the PR description checklist and land in
the same follow-up commit that flips this scaffold to a shipping
build:

1. **Oracle JDK 21.0.5 tarball SHA-256.** Currently `SKIP` in
   `tools/java-rootfs/fetch.sh`. `fetch.sh` refuses to run under
   `NANOVM_STRICT_PINS=1` while `SKIP` is in place. Capture with:

   ```sh
   curl -fSL "https://download.oracle.com/java/21/archive/jdk-21.0.5_linux-x64_bin.tar.gz" \
       | sha256sum
   ```

2. **Spring Petclinic commit.** Currently a `PLACEHOLDER_PIN_BEFORE_MERGE`
   sentinel in `Dockerfile`'s `PETCLINIC_REF` build arg. Pin to a
   specific commit hash on the upstream `main` branch, e.g.:

   ```sh
   git ls-remote https://github.com/spring-projects/spring-petclinic.git main
   ```

3. **Debian 12 slim digest.** Same story — pin the actual `sha256:…`
   from the current tag. Both `FROM debian:12-slim@${DEBIAN_DIGEST}`
   references in the Dockerfile interpolate this arg so a single
   override covers both stages.

## What Milestone 1 explicitly does NOT include

- **The seven-service Petclinic-microservices variant** — Milestone 2.
- **Cross-guest networking / static hostname map** — Milestone 2
  (Eureka needs stable IP-to-hostname resolution across forks).
- **CRaC-based warmup coordination** — this milestone uses console-tail
  for the ready marker, which is simpler and covers the whole demo.
  Vsock signaling replaces it in a later milestone if profiling shows
  the console read adds meaningful latency.
- **ARM64 (aarch64) support** — this milestone targets x86_64 only,
  matching the enterprise-Java v1 platform scope in `CLAUDE.md`.
