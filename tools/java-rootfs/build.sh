#!/usr/bin/env bash
# Build the Petclinic Java rootfs, producing two artifacts:
#
#   1. cache/initramfs.cpio.gz  — the shape the KVM backend actually
#      boots today. `vm-kvm` already loads an initramfs at high guest
#      RAM (`load_initrd`) and points the kernel's boot params at it,
#      so no virtio-blk device is needed for the demo. Guest kernel
#      unpacks the cpio into rootfs, execs /sbin/init, and Petclinic
#      launches. MAP_PRIVATE fork of guest RAM automatically captures
#      the whole rootfs into each fork — the shape the fork-many
#      benchmark wants.
#
#   2. cache/rootfs.ext4        — the same content packed as an ext4
#      image. Kept for the eventual virtio-blk story (production-shape
#      disk-backed rootfs, arbitrary size, better fit for stateful
#      workloads). Not used by the current KVM boot path.
#
# Pipeline:
#   1. `fetch.sh cache/`         — pull the Oracle JDK 21 tarball (idempotent).
#   2. `docker buildx build`     — assemble the multi-stage image
#                                   described in `Dockerfile`.
#   3. Export the image filesystem via `docker export`.
#   4. Pack into initramfs.cpio.gz (RAM boot; the demo path).
#   5. Pack the SAME content into rootfs.ext4 (future virtio-blk).
#
# Environment overrides (all optional):
#   PETCLINIC_REF       Git ref/commit to build. Defaults to the Dockerfile
#                       sentinel — pin before release.
#   DEBIAN_DIGEST       Sha256 digest for debian:12-slim. Same story.
#   DOCKER_BIN          Path to docker (defaults to `docker`).
#   ROOTFS_MB           Ext4 size in MiB (default 450).
#   SKIP_EXT4=1         Skip the ext4 pack (needs root for the loopback
#                       mount); useful in CI or non-root environments.
#                       initramfs.cpio.gz always builds — no root needed.
#   TARGET_PLATFORM     Docker platform for the build (default
#                       linux/amd64). Oracle JDK ships x86-64 binaries;
#                       on Apple Silicon (M-series) hosts the buildx
#                       default of linux/arm64 would pick a mismatched
#                       base image and `java -Xshare:dump` would fail
#                       with exec-format error, so we pin.
#
# Host prerequisites:
#   - docker + buildx (any host, including Apple Silicon via Docker
#     Desktop's amd64 emulation)
#   - cpio, gzip, find, sudo (only sudo when packing ext4 — set
#     SKIP_EXT4=1 to skip that)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE_DIR="${SCRIPT_DIR}/cache"
IMAGE_TAG="nanovm-java-rootfs:local"
ROOTFS_MB="${ROOTFS_MB:-450}"
SKIP_EXT4="${SKIP_EXT4:-0}"
TARGET_PLATFORM="${TARGET_PLATFORM:-linux/amd64}"

DOCKER_BIN="${DOCKER_BIN:-docker}"

# Sanity-check the host tools upfront so a missing `cpio` fails the
# script here rather than mid-way through the (long) docker build.
for tool in cpio gzip find; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        echo "✗ required host tool not found: ${tool}" >&2
        echo "  install ${tool} (Linux: apt/yum, macOS: brew install ${tool})" >&2
        exit 2
    fi
done

mkdir -p "${CACHE_DIR}"

# ---- 1. fetch Oracle JDK --------------------------------------------
"${SCRIPT_DIR}/fetch.sh" "${CACHE_DIR}"

# ---- 2. multi-stage build -------------------------------------------
BUILD_ARGS=()
if [[ -n "${PETCLINIC_REF:-}" ]]; then
    BUILD_ARGS+=("--build-arg" "PETCLINIC_REF=${PETCLINIC_REF}")
fi
if [[ -n "${DEBIAN_DIGEST:-}" ]]; then
    BUILD_ARGS+=("--build-arg" "DEBIAN_DIGEST=${DEBIAN_DIGEST}")
fi

echo "➜ docker buildx build --platform ${TARGET_PLATFORM} ${IMAGE_TAG}"
"${DOCKER_BIN}" buildx build \
    --platform "${TARGET_PLATFORM}" \
    --load \
    --tag "${IMAGE_TAG}" \
    "${BUILD_ARGS[@]}" \
    -f "${SCRIPT_DIR}/Dockerfile" \
    "${SCRIPT_DIR}"

# ---- 3. export the fs into a scratch dir ----------------------------
STAGE_DIR="$(mktemp -d)"
trap 'rm -rf "${STAGE_DIR}"' EXIT

echo "➜ exporting image filesystem to ${STAGE_DIR}"
CID="$("${DOCKER_BIN}" create "${IMAGE_TAG}")"
"${DOCKER_BIN}" export "${CID}" | tar -x -C "${STAGE_DIR}"
"${DOCKER_BIN}" rm -f "${CID}" > /dev/null

# ---- 4. pack initramfs.cpio.gz --------------------------------------
# The kernel expects the cpio "newc" format (a.k.a. SVR4). Short-option
# form (`-o -H newc`) is what BOTH GNU cpio (Linux) and BSD cpio
# (macOS default) accept — the long options `--create --format=newc
# --quiet` are GNU-only and would break the Apple-Silicon workflow the
# platform matrix promises. `-0` in combination with `find -print0`
# handles whitespace-in-names safely; both cpios accept `-0` short.
# stderr goes to /dev/null to swallow the "N blocks" summary BSD cpio
# always prints; a real failure trips `set -e` via the pipe status
# check below.
OUT_CPIO="${CACHE_DIR}/initramfs.cpio.gz"
echo "➜ packing ${OUT_CPIO}"
rm -f "${OUT_CPIO}"
(
    cd "${STAGE_DIR}"
    set -o pipefail
    find . -print0 \
        | cpio -o -0 -H newc 2>/dev/null \
        | gzip -9 -c > "${OUT_CPIO}"
)
echo "✓ ${OUT_CPIO} ($(du -h "${OUT_CPIO}" | awk '{print $1}'))"

# ---- 5. pack rootfs.ext4 (optional; for the future virtio-blk path) --
if [[ "${SKIP_EXT4}" == "1" ]]; then
    echo "skipping rootfs.ext4 (SKIP_EXT4=1)"
else
    OUT_EXT4="${CACHE_DIR}/rootfs.ext4"
    echo "➜ building ${OUT_EXT4} (${ROOTFS_MB} MiB)"
    rm -f "${OUT_EXT4}"
    truncate -s "${ROOTFS_MB}M" "${OUT_EXT4}"
    mkfs.ext4 -q -F -L nanovm-java "${OUT_EXT4}"

    MNT="$(mktemp -d)"
    sudo mount -o loop "${OUT_EXT4}" "${MNT}"
    sudo cp -a "${STAGE_DIR}"/. "${MNT}"/
    sudo umount "${MNT}"
    rmdir "${MNT}"

    echo "✓ ${OUT_EXT4} ($(du -h "${OUT_EXT4}" | awk '{print $1}'))"
fi

echo
echo "Next: reuse vmlinux from tools/kvm-images/ and boot with the"
echo "KVM backend, passing initramfs.cpio.gz to VmConfig.initrd. See"
echo "docs/prototypes/petclinic.md for the demo run once the"
echo "nanovm-jvm-bench binary lands (Milestone 1 PR #269)."
