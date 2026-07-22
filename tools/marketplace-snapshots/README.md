# First-party marketplace snapshot publisher

Tooling for building and publishing the pre-warmed sandbox snapshots
that appear in the operator's [`NANOVM_MARKETPLACE_CONFIG`](../../deploy/marketplace/README.md).

The **fork endpoint** ([`crates/control-plane/src/marketplace_fork.rs`](../../crates/control-plane/src/marketplace_fork.rs))
is on main; what's missing is the **actual tarballs** those entries
point at. Every published snapshot follows the same three-step
pipeline:

1. **Build** — spin up a KVM VM from a base kernel + rootfs, install
   the runtime we're advertising (Python + pandas, Node + Playwright,
   …), warm it (`import pandas` / `require('playwright')` /
   `python -c "1+1"`), then snapshot the VM.
2. **Package** — `tar.gz` the `manifest.json` + backing file emitted
   under the control-plane's local snapshot store.
3. **Publish** — upload the tarball to a public HTTPS URL (S3
   bucket, Cloudflare R2 bucket, or your own CDN). Update
   `deploy/marketplace/example.json` to point at the new URL and
   commit.

## Catalogue

The initial first-party set:

| Name | Runtime | Warm-up cost | Compressed tarball |
|---|---|---|---|
| `alpine-3.20-shell` | Alpine 3.20 + busybox + coreutils + bash + curl + jq | ~6 MB | ~4 MB |
| `python-3.12-minimal` | Python 3.12 + stdlib only | ~12 MB | ~8 MB |
| `python-3.12-ds` | Python 3.12 + pandas + numpy + scikit-learn + matplotlib (pre-imported) | ~180 MB | ~55 MB |
| `node-20-playwright` | Node 20 LTS + Playwright + Chromium headless (pre-launched) | ~220 MB | ~85 MB |

The compressed size is what tenants download on first fork. Warmed
imports live in the snapshot's memory image, so the second fork is a
CoW clone (~12 ms) with pandas already in RAM.

## Prereqs

- `nanovm-control-plane` binary from a `--features kvm` build.
- `nanovm-fork-bench` binary (used by `publish.sh` for the smoke test).
- `/dev/kvm` (Linux nested-KVM host, AWS `*.metal`, Fly.io perf
  machine, or Lima on a Mac).
- `aws-cli` (or `rclone` if you're on Cloudflare R2).
- `NANOVM_MARKETPLACE_BUCKET` env var — the public S3-compatible
  bucket that hosts the tarballs. Defaults to `s3://cdn-nanovm-io`
  (change in `build.sh` for a fork).

## Build a snapshot

```bash
./tools/marketplace-snapshots/build.sh alpine-3.20-shell
```

Under the hood:
1. Boots `Dockerfile.<name>` into a fresh KVM VM.
2. SSHes in, runs the warm-up commands from `warmup.d/<name>.sh`.
3. Snapshots the VM (`POST /v1/vms/:id/snapshot`) — the control plane
   writes `manifest.json` + `memory.cow` into its snapshot dir.
4. Tars the snapshot dir into `./out/<name>.tar.gz`.

## Publish

```bash
./tools/marketplace-snapshots/publish.sh alpine-3.20-shell
```

- Uploads `./out/<name>.tar.gz` to `${NANOVM_MARKETPLACE_BUCKET}/<name>/snapshot.tar.gz`
  with `Content-Type: application/gzip` and public-read ACL.
- Prints the resulting URL — paste it into
  `deploy/marketplace/example.json` under the entry's `snapshot_url`.
- Runs a smoke test: a fresh `nanovm-fork-bench` fork against the
  published tarball, asserting the VM boots + the warm-up state is
  present (`python -c "import pandas; print(pandas.__version__)"`).

## What's NOT here

- **The CDN itself.** Bucket / distribution / access-log wiring is an
  operator concern — this repo publishes the tooling; you provide the
  bucket. See the `deploy/enterprise/README.md` support-boundary
  matrix.
- **Cross-arch builds.** The current pipeline is x86_64 only. arm64
  Firecracker + Alpine works but isn't wired in the scripts; open an
  issue if you need it.
- **Automated CI publishing.** Snapshots are curated content; a
  wrong-shape tarball is a customer-facing bug. Publishing goes
  through a human review + a manual `publish.sh` invocation on a
  trusted host. A `release.yml` that does this on git-tag push is
  possible but deliberately not shipped by default.

## Community snapshots

The `maintainer` field on each catalogue entry is either
`nanovm-marketplace` (first-party, this pipeline) or a community
publisher's handle. Community entries land under a separate
`marketplace/community.json` — see the "v4" roadmap item in
[`deploy/marketplace/README.md`](../../deploy/marketplace/README.md).
