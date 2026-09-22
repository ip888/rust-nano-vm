# `tools/java-rootfs/`

Builds a Debian-slim–based ext4 rootfs (`cache/rootfs.ext4`) containing:

- **Oracle JDK 21 LTS** (pinned point release), the JDK the target
  enterprise customer stack (Citibank et al.) standardises on.
- **Spring Petclinic** single-jar variant, built from a pinned commit
  of `spring-projects/spring-petclinic`.
- A minimal init (`/sbin/init` with a `/init` symlink) that launches
  the JVM on boot and starts an in-guest warmup driver
  (`warmup.sh`).

This is the **rootfs artifact half** of the Petclinic prototype's
Milestone 1. Actually booting it through the repository's `vm-kvm`
backend needs **two follow-up commits** on this branch before merge:

1. **virtio-blk device attachment in `crates/vm-kvm`** — today
   `KvmHypervisor::build_runtime` reads `VmConfig.kernel` /
   `VmConfig.cmdline` but does not consume `VmConfig.rootfs`. Booting
   from an ext4 image needs a virtio-blk device attached at
   `/dev/vda` plus `root=/dev/vda init=/sbin/init` on the kernel
   cmdline.
2. **`nanovm-jvm-bench`** — the host-side driver that snapshots the
   warmed guest, forks N times, and measures per-fork time to first
   HTTP-200.

The rootfs artifact this builder produces is **verifiable independently**
with `docker run` (see the smoke check in
[`docs/prototypes/petclinic.md`](../../docs/prototypes/petclinic.md))
so review of this piece isn't gated on the follow-ups.

## Why Debian, not Alpine

Oracle JDK 21 is glibc-linked. Alpine's musl userland breaks the JDK's
loader without a compatibility shim (`gcompat`), and shims are a known
source of subtle JVM misbehavior (JNI mismatches, hotspot crashes).
Debian 12 (bookworm) slim adds ~20 MiB over Alpine minirootfs but
avoids the whole class of issue.

Total rootfs size target: **≤ 450 MiB uncompressed ext4**
(`ROOTFS_MB=450` default in `build.sh`). Rough breakdown: Debian slim
base (~50 MiB after doc/man/locale strip) + Oracle JDK 21
(~180 MiB after `jmods`/`legal`/`man` strip + CDS dump) + Petclinic
uber-jar (~70 MiB) + curl/iproute2/procps + init + warmup scripts
(< 5 MiB). Raise `ROOTFS_MB` if a future JDK point release exceeds
this budget.

## Build

Requires Docker with `buildx`, `mkfs.ext4` (from `e2fsprogs`), and
~1 GiB of free disk in `cache/`.

```sh
tools/java-rootfs/build.sh
```

Outputs `tools/java-rootfs/cache/rootfs.ext4`. Once the vm-kvm
virtio-blk follow-up lands, this file becomes the KVM backend's
`--rootfs`; today it can be smoke-checked via `docker run` per the
reproduce doc.

## Pins

| Component | Pinned to | Verified with |
|---|---|---|
| Oracle JDK | `21.0.5` (`jdk-21.0.5_linux-x64_bin.tar.gz`) | `sha256` in `fetch.sh` (currently `SKIP`; fetch script refuses to run under `NANOVM_STRICT_PINS=1` until this flips to a real digest) |
| Spring Petclinic | commit specified in `Dockerfile` `ARG PETCLINIC_REF` | Git checkout by commit hash — no CDN in the loop |
| Debian base | `debian:12-slim@sha256:…` — `ARG DEBIAN_DIGEST` interpolated into both `FROM` lines | Digest — a bad pin fails the build at `docker buildx` time |

The `Dockerfile` accepts overrides via `--build-arg` so an operator on
a hardened image registry can substitute in-cluster mirrors.
