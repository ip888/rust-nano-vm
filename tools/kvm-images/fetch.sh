#!/bin/sh
# Fetch a Firecracker sample kernel + rootfs pair suitable for booting a
# Linux microVM.
#
# Currently the SHA-256 checksums below are set to `SKIP` — this is a
# bootstrap posture, NOT a shipping posture. Pinning real digests is a
# follow-up: capture the checksums from a known-good run, wire them
# into `KERNEL_SHA`/`ROOTFS_SHA` below (see the comment on those
# variables), and bump `SCHEMA_VERSION` so a stale Docker cache
# rebuilds. Until then, a compromised CDN could substitute the image.
#
# Called from `Dockerfile.kvm`'s builder stage. Can also be run by an
# operator on the host to populate a directory the KVM binary is
# pointed at via `NANOVM_DEFAULT_KERNEL_PATH` / `NANOVM_DEFAULT_ROOTFS_PATH`.
#
# Usage:
#   ./fetch.sh <target-dir>
#
# Downloads:
#   $target/vmlinux       — Firecracker "hello" sample kernel (~5 MiB)
#   $target/rootfs.ext4   — Firecracker "hello" sample Alpine rootfs (~50 MiB)
#
# The "hello" URLs have been stable at s3.amazonaws.com/spec.ccfc.min
# since 2018 and are what the Firecracker "getting started" guide
# uses. If AWS ever rotates them, update the URLs + checksums below
# and bump `SCHEMA_VERSION` so a stale Docker cache rebuilds.

set -eu

SCHEMA_VERSION=1

# Pinned URLs. The `*_SHA` values are set to `SKIP` intentionally: this
# is the bootstrap posture. Capture the real digest with:
#   curl -L "$URL" | sha256sum
# then replace `SKIP` here and bump `SCHEMA_VERSION`. `fetch_verify`
# treats `SKIP` as "warn + continue" and any hex value as "enforce".
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/img/hello/kernel/hello-vmlinux.bin"
KERNEL_SHA="SKIP"
ROOTFS_URL="https://s3.amazonaws.com/spec.ccfc.min/img/hello/fsfiles/hello-rootfs.ext4"
ROOTFS_SHA="SKIP"

if [ $# -ne 1 ]; then
    printf 'usage: %s <target-dir>\n' "$0" >&2
    exit 2
fi
TARGET="$1"
mkdir -p "$TARGET"

fetch_verify() {
    url="$1"
    expected="$2"
    dest="$3"
    printf '➜ fetching %s\n' "$url" >&2
    # -L follows redirects; -f exits non-zero on 4xx/5xx so a stale
    # URL fails the build loudly rather than shipping an empty file.
    if ! curl -fSL --retry 3 --retry-delay 2 -o "$dest.tmp" "$url"; then
        printf '✗ failed to fetch %s\n' "$url" >&2
        rm -f "$dest.tmp"
        return 1
    fi
    if [ -n "$expected" ] && [ "$expected" != "SKIP" ]; then
        actual=$(sha256sum "$dest.tmp" | awk '{print $1}')
        if [ "$actual" != "$expected" ]; then
            printf '✗ checksum mismatch for %s\n  expected: %s\n  got:      %s\n' \
                "$url" "$expected" "$actual" >&2
            rm -f "$dest.tmp"
            return 1
        fi
        printf '✓ sha256 verified\n' >&2
    else
        printf '! sha256 verification SKIPPED (pin the checksum before merging)\n' >&2
    fi
    mv "$dest.tmp" "$dest"
    printf '✓ %s (%s)\n' "$dest" "$(du -h "$dest" | awk '{print $1}')" >&2
}

# See the header comment on `KERNEL_SHA`/`ROOTFS_SHA` for the pinning
# workflow. Threading the variables through here (rather than hard-coding
# `SKIP`) means flipping to real digests is a one-line change at the top.
fetch_verify "$KERNEL_URL" "$KERNEL_SHA" "$TARGET/vmlinux"
fetch_verify "$ROOTFS_URL" "$ROOTFS_SHA" "$TARGET/rootfs.ext4"

printf '\ndone. schema version: %s\n' "$SCHEMA_VERSION" >&2
