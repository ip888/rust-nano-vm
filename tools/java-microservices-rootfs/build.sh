#!/usr/bin/env bash
# Build the Petclinic-Microservices Java rootfs. Same shape as the M1
# single-jar `tools/java-rootfs/build.sh` — see the header comments
# there for the pipeline rationale.
#
# Pipeline:
#   1. `fetch.sh cache/`        — pull Oracle JDK 21 (idempotent).
#   2. `docker buildx build`    — assemble multi-stage image with all
#                                  seven Petclinic microservice jars
#                                  + JDK + init/warmup/tier-launcher.
#   3. `docker export` → tar     — extract the image filesystem.
#   4. `cpio -o -H newc | gzip`  — pack initramfs.cpio.gz.
#   5. Optional `mkfs.ext4`      — same content in ext4 for future
#                                  virtio-blk use (SKIP_EXT4=1 skips).
#
# Env overrides:
#   PETCLINIC_MS_REF     Git ref/commit of spring-petclinic-microservices.
#                        Defaults to the Dockerfile sentinel — pin
#                        before any release-track branch.
#   DEBIAN_DIGEST        Sha256 digest for debian:12-slim.
#   TARGET_PLATFORM      Docker platform (default linux/amd64 for
#                        Oracle JDK compatibility, see M1 for why).
#   DOCKER_BIN           Path to docker (defaults to `docker`).
#   ROOTFS_MB            Ext4 size in MiB (default 1200 — larger than
#                        M1's 450 to fit all seven services).
#   SKIP_EXT4=1          Skip the ext4 pack.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE_DIR="${SCRIPT_DIR}/cache"
IMAGE_TAG="nanovm-java-microservices-rootfs:local"
ROOTFS_MB="${ROOTFS_MB:-1200}"
SKIP_EXT4="${SKIP_EXT4:-0}"
TARGET_PLATFORM="${TARGET_PLATFORM:-linux/amd64}"

DOCKER_BIN="${DOCKER_BIN:-docker}"

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
if [[ -n "${PETCLINIC_MS_REF:-}" ]]; then
    BUILD_ARGS+=("--build-arg" "PETCLINIC_MS_REF=${PETCLINIC_MS_REF}")
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

# ---- 5. pack rootfs.ext4 --------------------------------------------
if [[ "${SKIP_EXT4}" == "1" ]]; then
    echo "skipping rootfs.ext4 (SKIP_EXT4=1)"
else
    OUT_EXT4="${CACHE_DIR}/rootfs.ext4"
    echo "➜ building ${OUT_EXT4} (${ROOTFS_MB} MiB)"
    rm -f "${OUT_EXT4}"
    truncate -s "${ROOTFS_MB}M" "${OUT_EXT4}"
    mkfs.ext4 -q -F -L nanovm-java-ms "${OUT_EXT4}"

    MNT="$(mktemp -d)"
    sudo mount -o loop "${OUT_EXT4}" "${MNT}"
    sudo cp -a "${STAGE_DIR}"/. "${MNT}"/
    sudo umount "${MNT}"
    rmdir "${MNT}"

    echo "✓ ${OUT_EXT4} ($(du -h "${OUT_EXT4}" | awk '{print $1}'))"
fi

echo
echo "Next: reuse vmlinux from tools/kvm-images/, boot with the KVM"
echo "backend passing initramfs.cpio.gz to VmConfig.initrd. Guest boots"
echo "the whole seven-service stack in one guest by default; a future"
echo "virtio-net-bridge PR will let NANOVM_TIER=infra|app split it"
echo "across two guests. See docs/prototypes/petclinic-microservices.md."
