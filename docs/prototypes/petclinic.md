# Petclinic prototype (Milestone 1)

The canonical demo target for the enterprise-Java product line:
snapshot a warmed Spring Petclinic inside a nanovm KVM guest, fork it
many times, measure per-fork time to first HTTP-200 on `/vets`.

The **single-jar Petclinic** variant is scoped for this milestone.
The seven-service Spring-Petclinic-Microservices variant lands as
Milestone 2 (separate PR).

## What the demo shows

Two side-by-side numbers on the same rootfs:

| Snapshot taken | Restore path | Expected `time-to-first-HTTP-200` p50 |
|---|---|---|
| **Cold** — right after JVM boot, before Spring context finishes loading | Spring finishes init in the forked child | ~500 ms |
| **Warm** — after the in-guest warmup driver hits `/actuator/health` + JIT warmup | First request served from restored state | **~150 ms** |

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
# 1. Fetch Firecracker sample kernel (reused).
tools/kvm-images/fetch.sh tools/java-rootfs/cache

# 2. Build the Java rootfs (Oracle JDK 21 + Petclinic).
tools/java-rootfs/build.sh
```

Outputs:

- `tools/java-rootfs/cache/vmlinux` — Firecracker sample kernel
- `tools/java-rootfs/cache/rootfs.ext4` — Debian slim + Oracle JDK 21
  + Petclinic uber-jar + init + warmup

Total rootfs size: ≤ 450 MiB uncompressed.

### 3. Boot manually to sanity-check

```sh
cargo run --features kvm -p cli -- vm run \
    --kernel tools/java-rootfs/cache/vmlinux \
    --rootfs tools/java-rootfs/cache/rootfs.ext4 \
    --vcpus 2 --memory-mib 1536
```

Watch stdio. Expected sequence:

```
[init] launching JVM: java -Xmx1g -Xshare:auto -XX:+UseZGC ... -jar /opt/petclinic.jar
[init] JVM pid=42
[warmup] waiting up to 60s for http://127.0.0.1:8080/actuator/health
[warmup] health 200 after 6s
[warmup] jit warmup: 3 passes across the demo endpoints
[warmup] pass 1 done
[warmup] pass 2 done
[warmup] pass 3 done
NANOVM_PETCLINIC_READY
```

Cold-boot-to-ready in the guest: 5–10 seconds. That's the baseline the
fork numbers replace.

### 4. Run the snapshot-fork benchmark

*(This step lands together with the `nanovm-jvm-bench` binary — a
follow-up commit inside Milestone 1. The command below is the final
interface; the rootfs bits above already work on their own.)*

```sh
cargo run --release --features kvm -p bench --bin nanovm-jvm-bench -- \
    --kernel  tools/java-rootfs/cache/vmlinux \
    --rootfs  tools/java-rootfs/cache/rootfs.ext4 \
    --snapshot-at both \
    --forks 20 --warmup 5
```

Emits a two-column markdown table with p50 / p90 / p99 for both the
cold- and warm-snapshot paths, plus a histogram.

## Pinning workflow

Two moving pieces need real values before the first PR that carries
this to `main`:

1. **Oracle JDK 21.0.5 tarball SHA-256.** Currently `SKIP` in
   `tools/java-rootfs/fetch.sh`. Capture with:

   ```sh
   curl -fSL "https://download.oracle.com/java/21/archive/jdk-21.0.5_linux-x64_bin.tar.gz" \
       | sha256sum
   ```

   Then replace `SKIP` in `fetch.sh` and bump its `SCHEMA_VERSION`.

2. **Spring Petclinic commit.** Currently a placeholder in
   `Dockerfile`'s `PETCLINIC_REF` build arg. Pin to a specific commit
   hash on the upstream `main` branch, e.g.:

   ```sh
   git ls-remote https://github.com/spring-projects/spring-petclinic.git main
   ```

   then replace `PLACEHOLDER_PIN_BEFORE_MERGE`.

3. **Debian 12 slim digest.** Same story — pin the actual `sha256:…`
   from the current tag.

Both are called out in the PR description checklist.

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
