#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "java-demo: Linux + /dev/kvm are required" >&2
    exit 2
fi
if [[ ! -e /dev/kvm ]]; then
    echo "java-demo: /dev/kvm is unavailable" >&2
    exit 2
fi

for tool in cargo docker cpio gzip; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "java-demo: required tool not found: $tool" >&2
        exit 2
    }
done

if [[ -n "${NANOVM_DEMO_SOCKET_DIR:-}" ]]; then
    SOCKET_DIR="$NANOVM_DEMO_SOCKET_DIR"
    CLEAN_SOCKET_DIR=0
else
    SOCKET_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nanovm-java-demo.XXXXXX")"
    CLEAN_SOCKET_DIR=1
fi
mkdir -p "$SOCKET_DIR"
cleanup() {
    if [[ "$CLEAN_SOCKET_DIR" == "1" ]]; then
        rm -rf -- "$SOCKET_DIR"
    fi
}
trap cleanup EXIT

CHILD_PID=""

run_child() {
    "$@" &
    CHILD_PID=$!
    local status
    if wait "$CHILD_PID"; then
        status=0
    else
        status=$?
    fi
    CHILD_PID=""
    return "$status"
}

cancel() {
    local signal="$1"
    local status="$2"
    trap - INT TERM
    if [[ -n "$CHILD_PID" ]]; then
        kill -"$signal" "$CHILD_PID" 2>/dev/null || true
        wait "$CHILD_PID" 2>/dev/null || true
        CHILD_PID=""
    fi
    cleanup
    exit "$status"
}
trap 'cancel INT 130' INT
trap 'cancel TERM 143' TERM

echo "java-demo: building JVM kernel"
run_child env KERNEL_PROFILE=jvm tools/kernel/build-tiny-kernel.sh

echo "java-demo: building pinned Petclinic initramfs"
run_child env NANOVM_STRICT_PINS=1 SKIP_EXT4=1 tools/java-rootfs/build.sh

echo "java-demo: building production KVM fleet binaries"
run_child cargo build --release --locked -p nanovm-jailer --bin nanovm-jailer
run_child cargo build --release --locked \
    --features kvm,virtio-net-smoltcp \
    -p nanovm-vmm-child --bin nanovm-vmm-child
run_child cargo build --release --locked \
    --features kvm \
    -p bench --bin nanovm-java-demo

echo "java-demo: running warmed snapshot/fork HTTP flow"
run_child target/release/nanovm-java-demo \
    --kernel tools/kernel/cache/bzImage-jvm \
    --initramfs tools/java-rootfs/cache/initramfs.cpio.gz \
    --jailer "$ROOT/target/release/nanovm-jailer" \
    --vmm-child "$ROOT/target/release/nanovm-vmm-child" \
    --socket-dir "$SOCKET_DIR" \
    "$@"
