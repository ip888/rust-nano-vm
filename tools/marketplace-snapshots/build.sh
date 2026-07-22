#!/usr/bin/env bash
# Build one marketplace snapshot tarball locally.
#
# Usage: ./build.sh <snapshot-name>
#
# Reads:
#   tools/marketplace-snapshots/snapshots/<name>/Dockerfile   — the base image
#   tools/marketplace-snapshots/snapshots/<name>/warmup.sh    — commands to run before snapshot
#   tools/marketplace-snapshots/snapshots/<name>/meta.json    — labels + description shown in the marketplace
#
# Writes:
#   tools/marketplace-snapshots/out/<name>.tar.gz             — the tarball publish.sh uploads
#   tools/marketplace-snapshots/out/<name>.meta.json          — merged metadata for the catalogue entry
#
# Requires:
#   - docker with buildx (for the rootfs build)
#   - /dev/kvm (nested-KVM host)
#   - nanovm-control-plane binary (built with --features kvm) on PATH
#     OR NANOVM_CONTROL_PLANE_BIN pointing at it
#   - jq
#
# Deliberately NOT idempotent per-invocation: every run starts a fresh
# VM, warms it, snapshots. Snapshots are curated content; determinism
# comes from pinned base-image digests, not from build-tool
# incremental logic.

set -euo pipefail

info()  { printf '\033[36m➜\033[0m %s\n' "$*"; }
warn()  { printf '\033[33m!\033[0m %s\n' "$*" >&2; }
error() { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

if [ $# -ne 1 ]; then
    error "usage: $0 <snapshot-name>"
fi

NAME="$1"
HERE="$(cd "$(dirname "$0")" && pwd)"
SNAP_DIR="$HERE/snapshots/$NAME"
OUT_DIR="$HERE/out"

if [ ! -d "$SNAP_DIR" ]; then
    error "no snapshot definition at $SNAP_DIR"
fi
for f in Dockerfile warmup.sh meta.json; do
    if [ ! -f "$SNAP_DIR/$f" ]; then
        error "missing $SNAP_DIR/$f"
    fi
done

if ! command -v docker >/dev/null 2>&1; then
    error "docker not on PATH"
fi
if ! command -v jq >/dev/null 2>&1; then
    error "jq not on PATH"
fi
if [ ! -c /dev/kvm ]; then
    error "/dev/kvm not present — need a nested-KVM host (AWS *.metal, Fly.io perf, Lima on Mac)"
fi

CP_BIN="${NANOVM_CONTROL_PLANE_BIN:-nanovm-control-plane}"
if ! command -v "$CP_BIN" >/dev/null 2>&1; then
    error "control-plane binary '$CP_BIN' not on PATH; build it with --features kvm or set NANOVM_CONTROL_PLANE_BIN"
fi

mkdir -p "$OUT_DIR"

# 1. Build the base rootfs image via docker → ext4.
info "building base rootfs for $NAME…"
BUILD_CTX="$(mktemp -d)"
trap 'rm -rf "$BUILD_CTX"' EXIT
cp "$SNAP_DIR/Dockerfile" "$BUILD_CTX/"
cp "$SNAP_DIR/warmup.sh"  "$BUILD_CTX/"

# `docker buildx build --output type=tar` emits an OCI tarball; we then
# extract its filesystem layer(s) into an ext4 image. Delegated to a
# helper because it's shared with tools/python-rootfs.
IMG_TAG="nanovm-marketplace-build:$NAME"
docker buildx build --tag "$IMG_TAG" --load "$BUILD_CTX" >&2

ROOTFS_EXT4="$OUT_DIR/$NAME.rootfs.ext4"
info "packing container fs → $ROOTFS_EXT4"
CID=$(docker create "$IMG_TAG")
trap 'docker rm -f "$CID" >/dev/null 2>&1 || true; rm -rf "$BUILD_CTX"' EXIT
ROOTFS_TAR="$(mktemp)"
docker export "$CID" > "$ROOTFS_TAR"

# Rough capacity: 2× the tarball size, min 128 MB.
ROOTFS_SIZE_MB=$(( ($(wc -c < "$ROOTFS_TAR") / 1024 / 1024) * 2 + 128 ))
truncate -s "${ROOTFS_SIZE_MB}M" "$ROOTFS_EXT4"
mkfs.ext4 -q -F "$ROOTFS_EXT4"
MNT="$(mktemp -d)"
sudo mount -o loop "$ROOTFS_EXT4" "$MNT"
sudo tar -xf "$ROOTFS_TAR" -C "$MNT"
sudo umount "$MNT"
rmdir "$MNT"
rm -f "$ROOTFS_TAR"

# 2. Boot a VM from the rootfs, warm it, snapshot.
info "booting VM for $NAME…"
CP_PORT=8180
SNAP_ROOT="$OUT_DIR/$NAME.snap"
rm -rf "$SNAP_ROOT"
mkdir -p "$SNAP_ROOT"

# Kernel is expected at NANOVM_DEFAULT_KERNEL_PATH or bundled at
# /usr/local/share/nanovm/vmlinux (see Dockerfile.kvm).
KERNEL="${NANOVM_MARKETPLACE_KERNEL:-/usr/local/share/nanovm/vmlinux}"
if [ ! -f "$KERNEL" ]; then
    error "kernel not found at $KERNEL; set NANOVM_MARKETPLACE_KERNEL"
fi

# Boot control-plane with a fresh snapshot dir so the emitted files
# land in a predictable spot.
CP_LOG="$OUT_DIR/$NAME.cp.log"
NANOVM_CONTROL_PLANE_ADDR="127.0.0.1:$CP_PORT" \
NANOVM_API_TOKENS="build:build-token" \
NANOVM_SNAPSHOT_STORE="file://$SNAP_ROOT" \
    "$CP_BIN" > "$CP_LOG" 2>&1 &
CP_PID=$!
trap 'kill $CP_PID 2>/dev/null || true; docker rm -f "$CID" >/dev/null 2>&1 || true; rm -rf "$BUILD_CTX"' EXIT

# Wait for the health check to come up.
for _ in $(seq 1 40); do
    if curl -fs "http://127.0.0.1:$CP_PORT/healthz" >/dev/null 2>&1; then
        break
    fi
    sleep 0.25
done

# Create VM.
VM_JSON=$(curl -fs -X POST "http://127.0.0.1:$CP_PORT/v1/vms" \
    -H "Authorization: Bearer build-token" \
    -H "Content-Type: application/json" \
    -d "$(jq -n --arg kernel "$KERNEL" --arg rootfs "$ROOTFS_EXT4" \
        '{kernel:$kernel, rootfs:$rootfs, vcpus:1, memory_mib:512, cmdline:"console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw quiet"}')")
VM_ID=$(echo "$VM_JSON" | jq -r .id)
info "VM $VM_ID created; starting"

curl -fs -X POST "http://127.0.0.1:$CP_PORT/v1/vms/$VM_ID/start" \
    -H "Authorization: Bearer build-token" >/dev/null

# 3. Warm-up: run the guest commands via the exec endpoint.
info "warming with $SNAP_DIR/warmup.sh"
while read -r cmd; do
    [ -z "$cmd" ] && continue
    case "$cmd" in \#*) continue ;; esac
    info "  guest: $cmd"
    curl -fs -X POST "http://127.0.0.1:$CP_PORT/v1/vms/$VM_ID/exec" \
        -H "Authorization: Bearer build-token" \
        -H "Content-Type: application/json" \
        -d "$(jq -n --arg cmd "$cmd" '{cmd:"sh", args:["-c", $cmd], timeout_ms: 60000}')" \
        >/dev/null
done < "$SNAP_DIR/warmup.sh"

# 4. Snapshot + pack.
info "snapshotting VM"
SNAP_JSON=$(curl -fs -X POST "http://127.0.0.1:$CP_PORT/v1/vms/$VM_ID/snapshot" \
    -H "Authorization: Bearer build-token" \
    -H "Content-Type: application/json" -d '{}')
SNAP_ID=$(echo "$SNAP_JSON" | jq -r .id)
info "snapshot id=$SNAP_ID; exporting to $SNAP_ROOT"

# Give the control-plane time to flush the snapshot dir.
sleep 1
kill "$CP_PID"
wait "$CP_PID" 2>/dev/null || true

# Pack.
SNAP_SUBDIR="$SNAP_ROOT/snap-$SNAP_ID"
if [ ! -d "$SNAP_SUBDIR" ]; then
    error "expected snapshot dir $SNAP_SUBDIR not present; see $CP_LOG"
fi
tar -C "$SNAP_SUBDIR" -czf "$OUT_DIR/$NAME.tar.gz" .
info "packed $(du -h "$OUT_DIR/$NAME.tar.gz" | cut -f1) → $OUT_DIR/$NAME.tar.gz"

# Emit the catalogue entry with real size_bytes.
SIZE_BYTES=$(stat -c %s "$OUT_DIR/$NAME.tar.gz")
jq --arg name "$NAME" --arg size "$SIZE_BYTES" \
    '. + {name: $name, size_bytes: ($size | tonumber), maintainer: "nanovm-marketplace"}' \
    "$SNAP_DIR/meta.json" > "$OUT_DIR/$NAME.meta.json"

info "done. Next: ./tools/marketplace-snapshots/publish.sh $NAME"
