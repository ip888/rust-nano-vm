# `tools/java-rootfs/`

Builds a Debian-slim–based rootfs containing:

- **Oracle JDK 21 LTS** (pinned point release), the JDK the target
  enterprise customer stack (Citibank et al.) standardises on.
- **Spring Petclinic** single-jar variant, built from a pinned commit
  of `spring-projects/spring-petclinic`.
- A minimal init (`/sbin/init` with a `/init` symlink) that launches
  the JVM on boot and starts an in-guest warmup driver
  (`warmup.sh`).

Two build artifacts land in `cache/`, both from the exact same
staged filesystem:

| Artifact | Purpose | Booted by |
|---|---|---|
| **`initramfs.cpio.gz`** | RAM-backed rootfs the demo uses today | vm-kvm's existing `VmConfig.initrd` path |
| **`rootfs.ext4`** | Disk-backed rootfs for the eventual virtio-blk story | Not consumed by the current backend |

## Why initramfs, not virtio-blk (for the demo)

Two reasons:

1. **vm-kvm already loads initramfs.** `VmConfig.initrd` is fully
   plumbed — `load_initrd()` reads bytes into high guest RAM, boot
   params get `ramdisk_image` / `ramdisk_size` set. No new device
   code needed for the demo.
2. **MAP_PRIVATE fork of guest RAM captures the whole rootfs for free.**
   When the initramfs unpacks into a tmpfs at `/`, that tmpfs lives
   in guest memory. Our fork mechanism does copy-on-write on guest
   RAM pages — so each forked child inherits an identical, private
   rootfs at zero copy cost. That's the shape the sub-second
   fork-many demo wants.

Virtio-blk stays on the roadmap for a production-shape story (larger
disk-backed rootfs, arbitrary size, stateful workloads). It's a
substantial addition to `crates/vm-kvm` (~500-1000 LOC of virtio
state machine + MMIO trap handling) and doesn't unblock the demo.

## Why Debian, not Alpine

Oracle JDK 21 is glibc-linked. Alpine's musl userland breaks the JDK's
loader without a compatibility shim (`gcompat`), and shims are a known
source of subtle JVM misbehavior (JNI mismatches, hotspot crashes).
Debian 12 (bookworm) slim adds ~20 MiB over Alpine minirootfs but
avoids the whole class of issue.

## Sizes

- **`initramfs.cpio.gz`** — expected ~150 MiB compressed (JDK + jar
  are the bulk; cpio + gzip -9 give ~2:1 ratio on already-compressed
  content). Unpacks to ~450 MiB in guest RAM.
- **`rootfs.ext4`** — 450 MiB uncompressed (`ROOTFS_MB=450` default).
  Raise it if a future JDK point release outgrows the budget.

Because the initramfs unpacks to ~450 MiB in guest RAM, size guest
memory to at least **2 GiB** (`VmConfig.memory_mib = 2048`) — 450 MiB
for the unpacked rootfs + 1 GiB for the JVM heap + kernel + slack.

## Build

Requires Docker with `buildx`. `mkfs.ext4` + `sudo` are only needed
for the optional ext4 pack — pass `SKIP_EXT4=1` to skip it in
non-root / CI environments; `initramfs.cpio.gz` builds without either.

```sh
tools/java-rootfs/build.sh
# or, for rootless / CI:
SKIP_EXT4=1 tools/java-rootfs/build.sh
```

## Pins

| Component | Pinned to | Verified with |
|---|---|---|
| Oracle JDK | `21.0.5` (`jdk-21.0.5_linux-x64_bin.tar.gz`) | `sha256` in `fetch.sh` (currently `SKIP`; fetch script refuses to run under `NANOVM_STRICT_PINS=1` until this flips to a real digest) |
| Spring Petclinic | commit specified in `Dockerfile` `ARG PETCLINIC_REF` | Git checkout by commit hash — no CDN in the loop |
| Debian base | `debian:12-slim@sha256:…` — `ARG DEBIAN_DIGEST` interpolated into both `FROM` lines | Digest — a bad pin fails the build at `docker buildx` time |

The `Dockerfile` accepts overrides via `--build-arg` so an operator on
a hardened image registry can substitute in-cluster mirrors.
