#!/bin/sh
# Fetch the Oracle JDK 21 tarball into cache/. Same source, same
# pinning workflow as `tools/java-rootfs/fetch.sh` — the microservices
# rootfs uses the exact same JDK build.
#
# See tools/java-rootfs/fetch.sh for the pinning-strictness contract
# (NANOVM_STRICT_PINS=1 fails closed on SKIP).

set -eu

SCHEMA_VERSION=1

JDK_VERSION="21.0.5"
JDK_URL="https://download.oracle.com/java/21/archive/jdk-${JDK_VERSION}_linux-x64_bin.tar.gz"
JDK_SHA="SKIP"

if [ $# -ne 1 ]; then
    printf 'usage: %s <target-dir>\n' "$0" >&2
    exit 2
fi
TARGET="$1"
mkdir -p "$TARGET"

if [ "${NANOVM_STRICT_PINS:-0}" = "1" ] && [ "$JDK_SHA" = "SKIP" ]; then
    printf '✗ NANOVM_STRICT_PINS=1 but JDK_SHA is SKIP\n' >&2
    printf '  refusing to fetch an unverified Oracle JDK tarball.\n' >&2
    printf '  pin the real sha256 in %s before running under strict mode.\n' "$0" >&2
    exit 3
fi

fetch_verify() {
    url="$1"
    expected="$2"
    dest="$3"
    printf '➜ fetching %s\n' "$url" >&2
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
        printf '! sha256 verification SKIPPED — do NOT ship this build\n' >&2
        printf '  (set NANOVM_STRICT_PINS=1 to fail closed on SKIP)\n' >&2
    fi
    mv "$dest.tmp" "$dest"
    printf '✓ %s (%s)\n' "$dest" "$(du -h "$dest" | awk '{print $1}')" >&2
}

fetch_verify "$JDK_URL" "$JDK_SHA" "$TARGET/jdk-${JDK_VERSION}_linux-x64_bin.tar.gz"

printf '\ndone. schema version: %s, jdk version: %s\n' \
    "$SCHEMA_VERSION" "$JDK_VERSION" >&2
