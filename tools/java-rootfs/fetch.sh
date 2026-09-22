#!/bin/sh
# Fetch the Oracle JDK 21 tarball into cache/.
#
# Oracle publishes numbered JDK tarballs at download.oracle.com/java/21/archive/
# under the No-Fee Terms and Conditions (NFTC) license. The URL scheme is
# stable across point releases; we pin to jdk-21.0.5.
#
# ## SHA-256 pinning is required for release builds
#
# The `JDK_SHA` below defaults to `SKIP` for the initial scaffold so a
# reviewer can exercise the fetch path without needing the real digest
# in hand. Any release-track build MUST pin the real digest and reject
# `SKIP`. This is enforced by `NANOVM_STRICT_PINS=1` at the top of the
# script — set it in CI / the shipping build so an accidental
# unpinned merge fails loudly rather than shipping an unverified
# Oracle blob into the rootfs.
#
# Capture the real digest with:
#
#   curl -fSL "$JDK_URL" | sha256sum
#
# then replace SKIP and bump SCHEMA_VERSION so a stale Docker cache
# rebuilds.
#
# Usage:
#   ./fetch.sh <target-dir>
#
# Downloads:
#   $target/jdk-21.0.5_linux-x64_bin.tar.gz  (~180 MiB compressed)

set -eu

SCHEMA_VERSION=1

# Pinned to a specific point release rather than "latest" so a rebuild
# a year from now yields byte-identical output.
JDK_VERSION="21.0.5"
JDK_URL="https://download.oracle.com/java/21/archive/jdk-${JDK_VERSION}_linux-x64_bin.tar.gz"
JDK_SHA="SKIP"

if [ $# -ne 1 ]; then
    printf 'usage: %s <target-dir>\n' "$0" >&2
    exit 2
fi
TARGET="$1"
mkdir -p "$TARGET"

# Enforce a real pin in strict mode (release CI, shipping builds).
# Interactive dev / initial scaffold review can leave STRICT_PINS
# unset to accept SKIP + warn.
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
