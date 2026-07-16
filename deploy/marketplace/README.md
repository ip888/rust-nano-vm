# Snapshot marketplace

Curated public registry of pre-built snapshot images an org can fork from — the wedge feature vs AWS Lambda MicroVMs, which only fork from a per-account image built via Dockerfile.

## Today (v1: read-side only)

`GET /v1/marketplace/snapshots` returns whatever is in `NANOVM_MARKETPLACE_CONFIG`. Unauthenticated so a dashboard / CLI / marketing page can browse before the visitor even signs up.

```bash
export NANOVM_MARKETPLACE_CONFIG=/path/to/marketplace.json
nanovm-control-plane
# in another shell:
curl -s http://localhost:8080/v1/marketplace/snapshots | jq '.snapshots | length'
```

Unset `NANOVM_MARKETPLACE_CONFIG` → endpoint returns `{"snapshots": []}`. Same posture as `NANOVM_PLAN_TIERS`: opt-in, no surprises.

## Config file shape

See [`example.json`](./example.json). Every entry:

```json
{
  "name": "python-3.12-ds",
  "description": "Short human-readable pitch.",
  "size_bytes": 52428800,
  "kernel_url": "https://cdn.example/marketplace/…/vmlinux",
  "rootfs_url": "https://cdn.example/marketplace/…/rootfs.ext4",
  "cmdline": "console=ttyS0 root=/dev/vda rw quiet",
  "labels": ["python", "data-science"],
  "maintainer": "nanovm-marketplace"
}
```

- `name` — URL-safe id matching `[a-z0-9][a-z0-9.-]*` **with no trailing `-` or `.`**. Allows natural versioned ids like `python-3.12-ds`; rejects `Uppercase`, `under_score`, `has/slash`. Becomes the path segment on the (future) fork endpoint. Invalid names log a `warn` and get skipped — a typo can't take the whole registry offline.
- `size_bytes` — approximate uncompressed rootfs size. Lets the dashboard render "~50 MB" without HEAD-ing the URL.
- `kernel_url` / `rootfs_url` — public HTTPS URLs. Empty either → entry skipped (logged).
- `cmdline` — passed verbatim to VMs forked from this snapshot.
- `labels` — free-form tags for filtering in a UI.
- `maintainer` — who publishes this entry. `"nanovm-marketplace"` for first-party.

## Roadmap

**v2 (next PR):** `POST /v1/marketplace/snapshots/:name/fork` — tenant-authed. Pulls the marketplace tarball into the tenant's snapshot store (via the existing `SnapshotStore` trait — filesystem or S3), then forks from it. Cached on first fetch; subsequent forks are ~12 ms warm-pool pops.

**v3:** Publish-side. `POST /v1/marketplace/snapshots` (admin-authed) — publish an org's own snapshot as a marketplace entry, either public (visible to all callers) or private (visible only to a specific org list).

**v4:** Community publishing. Anyone can propose a marketplace entry via GitHub PR against a `marketplace/community.json` in this repo. First-party curation gate stays first-party; community entries land in a separate `community` maintainer namespace.

## Why this matters (vs AWS Lambda MicroVMs)

AWS's MicroVM model requires you to bring a Dockerfile, they snapshot at image-build time, and you fork from that per-account image. **Their model can't do arbitrary-snapshot fork.** Ours can — and the marketplace is the discovery + delivery mechanism for pre-warmed snapshots that fork in ~12 ms.

For AI-agent inner loops (RL exploration, speculative branch execution) this is a fundamental capability gap AWS's shape doesn't cover.
