#!/bin/sh
# Fetch the Oracle JDK 21 tarball into cache/.
#
# Oracle publishes numbered JDK tarballs at download.oracle.com/java/21/archive/
# under the No-Fee Terms and Conditions (NFTC) license. The URL scheme is
# stable across point releases; we pin to jdk-21.0.5.
#
# The SHA-256 below is set to `SKIP` for the initial scaffold. Pin the
# real digest before merging any release-track branch:
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
        printf '! sha256 verification SKIPPED (pin the checksum before merging)\n' >&2
    fi
    mv "$dest.tmp" "$dest"
    printf '✓ %s (%s)\n' "$dest" "$(du -h "$dest" | awk '{print $1}')" >&2
}

fetch_verify "$JDK_URL" "$JDK_SHA" "$TARGET/jdk-${JDK_VERSION}_linux-x64_bin.tar.gz"

printf '\ndone. schema version: %s, jdk version: %s\n' \
    "$SCHEMA_VERSION" "$JDK_VERSION" >&2
