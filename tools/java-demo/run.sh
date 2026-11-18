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
trap cleanup EXIT INT TERM

echo "java-demo: building JVM kernel"
KERNEL_PROFILE=jvm tools/kernel/build-tiny-kernel.sh

echo "java-demo: building pinned Petclinic initramfs"
NANOVM_STRICT_PINS=1 SKIP_EXT4=1 tools/java-rootfs/build.sh

echo "java-demo: building production KVM fleet binaries"
cargo build --release --locked -p nanovm-jailer --bin nanovm-jailer
cargo build --release --locked \
    --features kvm,virtio-net-smoltcp \
    -p nanovm-vmm-child --bin nanovm-vmm-child
cargo build --release --locked \
    --features kvm \
    -p bench --bin nanovm-java-demo

echo "java-demo: running warmed snapshot/fork HTTP flow"
target/release/nanovm-java-demo \
    --kernel tools/kernel/cache/bzImage-jvm \
    --initramfs tools/java-rootfs/cache/initramfs.cpio.gz \
    --jailer "$ROOT/target/release/nanovm-jailer" \
    --vmm-child "$ROOT/target/release/nanovm-vmm-child" \
    --socket-dir "$SOCKET_DIR" \
    "$@"
