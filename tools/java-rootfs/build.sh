#!/usr/bin/env bash
# Build the Petclinic Java rootfs.
#
# Pipeline:
#   1. `fetch.sh cache/` — pull the Oracle JDK 21 tarball (idempotent).
#   2. `docker buildx build` — assemble the multi-stage image
#      described in `Dockerfile`.
#   3. Export the final image's filesystem via `docker export`.
#   4. Pack it into an ext4 image (`cache/rootfs.ext4`) sized to
#      hold the payload with ~20% slack.
#
# The rootfs.ext4 is what the KVM backend mounts as `--rootfs`. The
# companion `vmlinux` kernel is reused from `tools/kvm-images/` — the
# rootfs is Debian-flavoured but the kernel just needs virtio-blk,
# virtio-net, and ext4, which Firecracker's sample kernel already has.
#
# Environment overrides (all optional):
#   PETCLINIC_REF   Git ref/commit to build. Defaults to the pinned
#                   value in Dockerfile (currently a sentinel — pin
#                   before merging any release-track branch).
#   DEBIAN_DIGEST   Sha256 digest for debian:12-slim. Same story.
#   DOCKER_BIN      Path to docker (defaults to `docker`).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE_DIR="${SCRIPT_DIR}/cache"
IMAGE_TAG="nanovm-java-rootfs:local"
ROOTFS_MB="${ROOTFS_MB:-450}"   # target ext4 size in MiB

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

# ---- 4. pack into ext4 ----------------------------------------------
OUT="${CACHE_DIR}/rootfs.ext4"
echo "➜ building ${OUT} (${ROOTFS_MB} MiB)"
rm -f "${OUT}"
truncate -s "${ROOTFS_MB}M" "${OUT}"
mkfs.ext4 -q -F -L nanovm-java "${OUT}"

MNT="$(mktemp -d)"
sudo mount -o loop "${OUT}" "${MNT}"
sudo cp -a "${STAGE_DIR}"/. "${MNT}"/
sudo umount "${MNT}"
rmdir "${MNT}"

echo "✓ ${OUT} ($(du -h "${OUT}" | awk '{print $1}'))"
echo
echo "Next: reuse vmlinux from tools/kvm-images/ and boot with the"
echo "KVM backend. See docs/prototypes/petclinic.md for the demo run."
