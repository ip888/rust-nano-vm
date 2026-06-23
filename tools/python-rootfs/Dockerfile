# syntax=docker/dockerfile:1.6
# Build a Python-equipped initramfs for rust-nano-vm guests.
#
# Output: /initramfs-python.cpio.gz on the final scratch stage. The
# expected invocation is BuildKit with `--output type=local,dest=...`
# so the file lands on the host without needing to `docker create` +
# `docker cp`. `tools/python-rootfs/build.sh` is the canonical driver.
#
# Why Alpine: smallest mainstream distro with apk repositories. Its
# musl libc plays nicely with rust-nano-vm's musl agent binary, so we
# don't need glibc compatibility shims in the image.
#
# Why three stages:
#   1. rootfs  — the actual guest filesystem.
#   2. packer  — adds /dev nodes and packs everything into a cpio.
#   3. export  — empty scratch image so `--output type=local` writes
#                just the artifact, not the docker tooling layers.

# ---- Stage 1: assemble the rootfs -----------------------------------
FROM alpine:3.20 AS rootfs
RUN apk add --no-cache python3 \
    && rm -rf /usr/share/man /usr/share/doc /usr/share/info /var/cache/apk/*

# Pre-create the kernel-pseudo-FS mountpoints. Even if the tinyconfig
# kernel doesn't currently mount /proc, /sys, /tmp, having empty
# directories at these paths means a future agent change (or a script
# init) can mount them without ENOENT.
RUN mkdir -p /proc /sys /tmp /run

# `nanovm-agent` is the static-musl binary built on the host and
# passed as Docker context by build.sh. It runs as /init (the kernel
# is invoked with `rdinit=/init`), connects out to the host over
# AF_VSOCK, and serves Exec requests. Made executable here so we
# don't depend on the host's file modes.
COPY agent-musl /init
RUN chmod 755 /init

# ---- Stage 2: pack the rootfs into a cpio -----------------------------
# Done in a separate stage so the layer-set that ends up in the
# archive is exactly the rootfs, with no buildkit overlays.
FROM alpine:3.20 AS packer
RUN apk add --no-cache cpio gzip findutils
COPY --from=rootfs / /rootfs/
# /dev nodes aren't part of any Docker image's filesystem (docker
# mounts /dev at runtime). Create them now so init.c-style early
# userspace has /dev/console + /dev/null without depending on
# devtmpfs auto-mount (which the tinyconfig kernel doesn't enable).
RUN mkdir -p /rootfs/dev \
    && mknod /rootfs/dev/console c 5 1 \
    && mknod /rootfs/dev/null    c 1 3 \
    && mknod /rootfs/dev/zero    c 1 5 \
    && mknod /rootfs/dev/kmsg    c 1 11 \
    && chmod 600 /rootfs/dev/console \
    && chmod 666 /rootfs/dev/null /rootfs/dev/zero \
    && chmod 644 /rootfs/dev/kmsg
RUN cd /rootfs && find . | cpio --quiet -o -H newc | gzip -9 > /initramfs-python.cpio.gz

# ---- Stage 3: export-only image ---------------------------------------
# Empty scratch image carrying only the artifact, so a
# `docker build --output type=local,dest=cache` lands a single file at
# `cache/initramfs-python.cpio.gz` rather than a whole filesystem.
FROM scratch
COPY --from=packer /initramfs-python.cpio.gz /initramfs-python.cpio.gz
