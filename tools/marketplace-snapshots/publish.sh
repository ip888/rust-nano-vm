#!/usr/bin/env bash
# Publish one marketplace snapshot tarball to the configured CDN bucket.
#
# Usage: ./publish.sh <snapshot-name>
#
# Prereqs:
#   - `./build.sh <name>` completed → `out/<name>.tar.gz` exists.
#   - `NANOVM_MARKETPLACE_BUCKET` env var — S3 URI of the public bucket.
#     Defaults to `s3://cdn-nanovm-io` (change for your fork).
#   - `NANOVM_MARKETPLACE_BASE_URL` env var — public HTTPS base that
#     serves that bucket. Defaults to `https://cdn.nanovm.io`.
#   - `aws` CLI configured with credentials that can write the bucket.
#
# Writes:
#   - `<bucket>/<name>/snapshot.tar.gz` (Content-Type: application/gzip,
#     public-read ACL).
#
# Prints:
#   - The full `snapshot_url` to paste into `deploy/marketplace/example.json`
#     for the entry.
#
# The `example.json` is NOT patched automatically — deliberately so
# operators review + commit the catalogue change themselves.

set -euo pipefail

info()  { printf '\033[36m➜\033[0m %s\n' "$*"; }
error() { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

if [ $# -ne 1 ]; then
    error "usage: $0 <snapshot-name>"
fi

NAME="$1"
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT_DIR="$HERE/out"
TARBALL="$OUT_DIR/$NAME.tar.gz"
META="$OUT_DIR/$NAME.meta.json"

if [ ! -f "$TARBALL" ] || [ ! -f "$META" ]; then
    error "run ./build.sh $NAME first — $TARBALL or $META missing"
fi

BUCKET="${NANOVM_MARKETPLACE_BUCKET:-s3://cdn-nanovm-io}"
BASE_URL="${NANOVM_MARKETPLACE_BASE_URL:-https://cdn.nanovm.io}"

if ! command -v aws >/dev/null 2>&1; then
    error "aws CLI not on PATH; install and configure it, or swap for rclone in this script"
fi

DEST="${BUCKET}/marketplace/${NAME}/snapshot.tar.gz"
info "uploading $TARBALL → $DEST"
aws s3 cp "$TARBALL" "$DEST" \
    --content-type "application/gzip" \
    --acl public-read \
    --cache-control "public, max-age=86400, immutable"

URL="${BASE_URL}/marketplace/${NAME}/snapshot.tar.gz"
info "done. snapshot_url:"
printf '\n  %s\n\n' "$URL"

info "next: paste this into deploy/marketplace/example.json under the '$NAME' entry:"
printf '\n    "snapshot_url": "%s",\n\n' "$URL"

info "recommended smoke test after committing:"
printf '\n  curl -fs -X POST "https://api.nanovm.io/v1/marketplace/snapshots/%s/fork" \\\n' "$NAME"
printf '    -H "Authorization: Bearer <your-tenant-token>"\n\n'
