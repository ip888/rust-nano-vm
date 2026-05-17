# Changelog

All notable changes to `rust-nano-vm` will be recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
under the rules in [§ Versioning](#versioning) below.

## [Unreleased]

### Added
*(populated by each merged PR — entries land alongside the change)*

### Changed
### Fixed
### Removed
### Security

## Versioning

Pre-1.0 the project is in **active scaffolding**. SemVer applies as
follows during this period:

| Tier | Rule |
| --- | --- |
| `0.x.y` patch (`0.x.y → 0.x.y+1`) | Bug fixes, internal refactors, doc-only changes |
| `0.x.y` minor (`0.x.y → 0.x+1.0`) | New features, additive API/wire changes, new env vars with safe defaults |
| `0.x.y` major (`0.x.y → 0.y+1.0`) | Any **breaking** change to: the REST wire format, the `Hypervisor` trait, the on-disk snapshot format, an env var's default or semantics |

Once `1.0.0` ships the project switches to strict SemVer. Anything
under `pub` in `vm-core`, `proto`, and `control-plane`'s public
modules is then part of the stability contract. Internal modules
(`auth`, `error`, `request_id`, etc.) remain free to change at any
time even post-1.0.

### Operator-visible knobs are part of the contract

These are SemVer-tracked exactly like the API surface:

- HTTP routes under `/v1/*` (path, method, status codes, response
  envelope shape).
- `/healthz`, `/openapi.json`, `/metrics`.
- Every `NANOVM_*` environment variable's **name**, **default**, and
  **semantics**.
- Bearer-token format and the `Authorization: Bearer …` header
  convention.
- Structured error envelope `code` strings (the `code` field is the
  stable matcher; `message` is human-readable and free to change).

### Versioning of the on-disk snapshot format

The snapshot manifest (M5) carries an explicit `format_version`
integer. We **do not** silently upgrade — restoring an older
manifest with a newer binary returns
`VmError::Unsupported("snapshot format version N; this binary
supports M")`. Operators must run an explicit migration tool
(planned M5).

## Release process

1. Open a `release/0.x.y` branch from `main`.
2. Move every `[Unreleased]` entry into a new `[0.x.y] — YYYY-MM-DD`
   section. Empty subsections may be omitted.
3. Verify `cargo deny check`, `cargo test --workspace`, `cargo
   clippy --workspace --all-targets -- -D warnings`, `cargo fmt
   --all -- --check`.
4. Bump `version` in the workspace `Cargo.toml`. Update lockfile.
5. Tag `v0.x.y` on the merge commit. Push the tag.
6. The release workflow (planned) publishes the container image
   to `ghcr.io/ip888/nanovm-control-plane:0.x.y` and `:latest`.

## Conventions for entries

- Use the imperative mood: *"Add per-token rate limiter"*, not
  *"Added"*.
- Reference the PR number in parentheses at the end: `(#38)`.
- Group operator-visible changes (env vars, response shape, error
  codes) above internal refactors.
- For security-relevant changes, mirror the entry into
  `### Security` *and* the relevant tier above.

A future release-prep PR will likely automate population of this
file from PR titles via a workflow; until then, every PR that lands
should add a one-line entry to the appropriate `[Unreleased]`
subsection.
