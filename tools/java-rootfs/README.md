# `tools/java-rootfs/`

Builds a Debian-slim–based rootfs (`rootfs.ext4`) suitable for booting a
Linux microVM under the KVM backend, containing:

- **Oracle JDK 21 LTS** (pinned point release), the JDK the target
  enterprise customer stack (Citibank et al.) standardises on.
- **Spring Petclinic** single-jar variant, built from a pinned commit
  of `spring-projects/spring-petclinic`.
- A minimal init that launches the JVM on boot and starts an in-guest
  warmup driver (`warmup.sh`).

This is **Milestone 1** of the Petclinic prototype — a canonical demo
target for the sub-second Spring-Boot cold-start pitch.
See [`docs/prototypes/petclinic.md`](../../docs/prototypes/petclinic.md)
for the end-to-end run and expected metrics.

## Why Debian, not Alpine

Oracle JDK 21 is glibc-linked. Alpine's musl userland breaks the JDK's
loader without a compatibility shim (`gcompat`), and shims are a known
source of subtle JVM misbehavior (JNI mismatches, hotspot crashes).
Debian 12 (bookworm) slim adds ~20 MiB over Alpine minirootfs but
avoids the whole class of issue.

Total rootfs size target: **≤ 350 MiB uncompressed ext4**. Breakdown:
Debian slim base (~35 MiB) + Oracle JDK 21 (~180 MiB stripped) +
Petclinic uber-jar (~70 MiB) + init + warmup scripts (< 1 MiB).

## Build

Requires Docker with `buildx`, `mkfs.ext4` (from `e2fsprogs`), and
~1 GiB of free disk in `cache/`.

```sh
tools/java-rootfs/build.sh
```

Outputs:
- `tools/java-rootfs/cache/rootfs.ext4` — the ready-to-boot rootfs
- `tools/java-rootfs/cache/vmlinux`     — reused from `tools/kvm-images/`

Boot it via the KVM backend with:

```sh
cargo run --features kvm -p bench --bin nanovm-jvm-bench -- \
    --kernel tools/java-rootfs/cache/vmlinux \
    --rootfs tools/java-rootfs/cache/rootfs.ext4 \
    --snapshot-at both --forks 10
```

(The `nanovm-jvm-bench` binary is the Milestone 1 driver added in a
follow-up commit; the rootfs itself boots and self-warms without it.)

## Pins

| Component | Pinned to | Verified with |
|---|---|---|
| Oracle JDK | 21.0.5 (`jdk-21.0.5_linux-x64_bin.tar.gz`) | `sha256` in `fetch.sh` (currently `SKIP`; pin before merging to main) |
| Spring Petclinic | commit specified in `Dockerfile` `ARG PETCLINIC_REF` | Git checkout by commit hash — no CDN in the loop |
| Debian base | `debian:12-slim` digest pinned in `Dockerfile` | Digest reference stored as `ARG DEBIAN_DIGEST` |

The `Dockerfile` accepts overrides via `--build-arg` so an operator on
a hardened image registry can substitute in-cluster mirrors.
