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

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE_DIR="${SCRIPT_DIR}/cache"
IMAGE_TAG="nanovm-java-rootfs:local"
ROOTFS_MB="${ROOTFS_MB:-450}"
SKIP_EXT4="${SKIP_EXT4:-0}"

DOCKER_BIN="${DOCKER_BIN:-docker}"

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

echo "➜ docker buildx build ${IMAGE_TAG}"
"${DOCKER_BIN}" buildx build \
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
# The kernel expects the cpio "newc" format (a.k.a. SVR4). `find | cpio -o`
# reads paths on stdin and writes the archive on stdout; `-D` sets the
# base dir so the archive entries are rooted at "/" rather than the
# scratch dir path. `-0` and `-print0` handle whitespace-in-names
# safely (unlikely in a Debian rootfs but zero-cost insurance).
OUT_CPIO="${CACHE_DIR}/initramfs.cpio.gz"
echo "➜ packing ${OUT_CPIO}"
rm -f "${OUT_CPIO}"
(
    cd "${STAGE_DIR}"
    find . -print0 \
        | cpio --null --create --format=newc --quiet \
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
