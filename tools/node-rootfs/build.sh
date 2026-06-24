#!/usr/bin/env bash
# Build a Node.js-equipped initramfs for rust-nano-vm guests.
#
# Output: tools/node-rootfs/cache/initramfs-node.cpio.gz
#
# Workflow:
#   1. Cross-build `nanovm-agent` for x86_64-unknown-linux-musl
#      (static, non-PIE — same flags `tools/python-rootfs/build.sh`
#      uses, for the same kernel-can't-exec-ET_DYN reason).
#   2. Stage the agent binary into this directory as `agent-musl` so
#      Docker's build context can pick it up.
#   3. Run `docker buildx build --output type=local,dest=cache` against
#      the Dockerfile here. The Dockerfile assembles an Alpine 3.20 +
#      Node.js rootfs, packs it as a cpio (newc) + gzip, and emits
#      `initramfs-node.cpio.gz` into ./cache/.
#   4. Clean up the staged agent binary.
#
# Requirements on the host:
#   - docker with buildx (Docker 20.10+; Docker Desktop on macOS has it)
#   - rustup with the `x86_64-unknown-linux-musl` target installed
#   - cargo (the workspace's pinned toolchain via `rust-toolchain.toml`)
#
# Run with:
#   tools/node-rootfs/build.sh
#
# Then on a Linux + KVM host (e.g. an i5 dev laptop with /dev/kvm):
#   cargo test -p vm-kvm --features kvm --test exec_node_boot -- --nocapture

set -euo pipefail

readonly HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly CACHE="${HERE}/cache"
readonly WORKSPACE="$(cd "${HERE}/../.." && pwd)"
readonly MUSL_TARGET="x86_64-unknown-linux-musl"
readonly AGENT_BIN="${WORKSPACE}/target/${MUSL_TARGET}/release/nanovm-agent"
readonly STAGED_AGENT="${HERE}/agent-musl"
readonly OUT="${CACHE}/initramfs-node.cpio.gz"

mkdir -p "${CACHE}"

# ---- preflight -------------------------------------------------------
command -v docker >/dev/null || {
    echo "node-rootfs: need docker (https://docs.docker.com/get-docker/)" >&2
    exit 1
}
if ! docker buildx version >/dev/null 2>&1; then
    echo "node-rootfs: docker buildx not available — install Docker 20.10+ or Docker Desktop" >&2
    exit 1
fi
if ! rustup target list --installed 2>/dev/null | grep -q "${MUSL_TARGET}"; then
    echo "node-rootfs: missing Rust target ${MUSL_TARGET}" >&2
    echo "node-rootfs:   rustup target add ${MUSL_TARGET}" >&2
    exit 1
fi

# ---- 1. cross-build nanovm-agent -------------------------------------
# `-C relocation-model=static` forces ET_EXEC (not ET_DYN); the
# tinyconfig kernel can't load a static-PIE init — execve returns
# ENOENT. Same constraint that drives `tools/python-rootfs/build.sh`.
echo "node-rootfs: building nanovm-agent (musl, non-PIE)..."
(
    cd "${WORKSPACE}"
    RUSTFLAGS="-C relocation-model=static" \
        cargo build -p guest-agent --target "${MUSL_TARGET}" --release
)

# ---- 2. stage the binary into the docker context ---------------------
cp "${AGENT_BIN}" "${STAGED_AGENT}"
# Always clean up the staged binary on exit — keeping a copy in the
# tree confuses git status and is just clutter.
trap 'rm -f "${STAGED_AGENT}"' EXIT

# ---- 3. docker build -------------------------------------------------
echo "node-rootfs: building rootfs image via docker buildx..."
DOCKER_BUILDKIT=1 docker buildx build \
    --output "type=local,dest=${CACHE}" \
    --progress=plain \
    "${HERE}"

# ---- 4. report ------------------------------------------------------
if [[ ! -f "${OUT}" ]]; then
    echo "node-rootfs: ERROR — docker build finished but ${OUT} is missing" >&2
    exit 1
fi
bytes="$(stat -c%s "${OUT}" 2>/dev/null || stat -f%z "${OUT}")"
echo
echo "node-rootfs: DONE"
echo "node-rootfs:   archive ${OUT} (${bytes} bytes)"
echo
echo "Run on a Linux + KVM host:"
echo "  cargo test -p vm-kvm --features kvm --test exec_node_boot -- --nocapture"
