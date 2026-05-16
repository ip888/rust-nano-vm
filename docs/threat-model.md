# Threat model

This document is the security framing for `rust-nano-vm`. It exists so
operators can decide whether to run the control plane in front of
real workloads, and so contributors writing new code can ask "does this
expand the attack surface?" against a written baseline rather than
ambient assumptions.

It is **not** a security audit. The project is pre-1.0; the threat
model will evolve. The current snapshot reflects M0–M1 + the M6
control-plane slice.

## Asset inventory

| Asset | Where it lives | Why it matters |
| --- | --- | --- |
| Guest memory | Host RAM, owned by the VMM process | Contains agent-controlled code, intermediate computation, and any secrets the operator pushed in |
| Snapshots | Filesystem under the snapshot dir | Whoever can read them can replay all in-guest state, including secrets |
| API bearer tokens | `NANOVM_API_TOKENS` env var + caller's client config | Anyone with one can create, snapshot, fork, and read files from any VM the control plane manages |
| Host kernel | `/dev/kvm`, host syscall surface | A guest escape → host code execution → full blast radius |
| Sibling sandboxes | Other VMs sharing the host | A successful escape from one tenant's sandbox should not reach another's |

## Trust boundaries

1. **Guest ↔ host.** The strongest boundary. Guest code is treated as
   fully hostile — it is, by construction, agent-generated code or
   adversary-supplied test inputs. Crossings: the KVM ioctl surface
   (vCPU run loop, MMIO exits), the virtio rings (queues live in
   guest memory; descriptors are validated before dereference), and
   vsock/fs RPC messages (deserialized by `proto`).
2. **Control-plane caller ↔ control plane.** Bearer-token
   authenticated. Tokens are opaque — no scopes, no per-resource ACL
   yet (see §Known gaps).
3. **Control plane ↔ host filesystem.** The control plane writes
   snapshot manifests and reads guest exec output. Path traversal in
   these calls is a host-side bug, not a guest-side one — handled
   inside the control plane.
4. **Operator ↔ machine.** Out of scope: the operator with shell on
   the host has root over everything. They are not an adversary in
   this model.

## Adversaries

| Adversary | Goal | Capabilities |
| --- | --- | --- |
| **Malicious guest payload** | Escape, persist, steal host secrets | Full code execution inside the guest; can craft arbitrary virtio descriptors, vsock messages, MSR writes, PIO/MMIO |
| **Malicious authenticated API caller** | Exfiltrate other tenants' state, exhaust host resources, escalate within the API | One or more valid bearer tokens; can issue any documented request |
| **Network attacker (no token)** | Forge a session, brute-force tokens, deny service | Can reach `:8080`; cannot read host memory, cannot terminate TLS the operator set up |
| **Compromised dependency** | Land code via supply chain | One of the workspace's transitive dependencies turns hostile in a future release |

## In-scope mitigations (today)

### Process & language
- `#![forbid(unsafe_code)]` across the workspace except `vm-kvm`,
  which scopes `unsafe` to KVM ioctl wrappers and guest-memory access
  with a written justification on each block.
- All wire-format decoding (control-plane JSON, virtio descriptors,
  vsock framing, snapshot manifests) goes through `serde` or
  hand-rolled parsers with fuzz harnesses under `fuzz/` (cargo-fuzz).
- Workspace lints: `clippy --all-targets -D warnings` in CI;
  `cargo-deny` enforces license + advisory + duplicate policy.

### Control plane
- Bearer-token auth (`NANOVM_API_TOKENS`). Empty → auth disabled with
  a `WARN` log; production deployments must set it.
- Per-token rate limit (`NANOVM_RATE_LIMIT_RPS`, default 100,
  configurable burst). A single misbehaving or leaked token cannot
  exhaust the backend. `rps = 0` disables with a `WARN`.
- Structured error envelopes — never leak internal `Debug` payloads
  or stack traces to clients.
- `/healthz` and `/openapi.json` are deliberately exempt from auth
  so monitoring + SDK generators don't need to hold a token.

### Hypervisor backend
- `vm-mock` is the default for tests; CI never touches `/dev/kvm`,
  so no test secret needs to land on a hypervisor host.
- `vm-kvm` is feature-gated and Linux-only; non-KVM builds cannot
  accidentally pull in the ioctl surface.
- The M1 bring-up uses fresh `KVM_CREATE_VM` per guest and never
  shares vCPU fds across guests — there is no path today for one
  guest to read another's KVM state.

### Build & supply chain
- `Cargo.lock` is committed; CI uses `--locked` for binary builds.
- Rust toolchain pinned via `rust-toolchain.toml` (matches the CI
  matrix and the Dockerfile builder).
- Docker runtime image is `gcr.io/distroless/cc-debian12:nonroot` —
  no shell, no package manager, uid 65532.

## Known gaps (tracked, not yet closed)

These are the items operators MUST treat as live risks until the
linked milestone or follow-up PR closes them.

| # | Gap | Mitigation while open | Plan |
| - | --- | --- | --- |
| G1 | No `/metrics` endpoint — operators can't see per-token rate-limit hits or queue depth | Tail server logs (rate-limit hits are warn-logged) | Track A2 |
| G2 | No request-id correlation header | None | Track A3 |
| G3 | No graceful-drain budget on shutdown — SIGTERM closes inflight requests | Drain via your load balancer before sending SIGTERM | Track A4 |
| G4 | No audit log of mutating API calls | Rely on `tracing` info lines | Track A5 |
| G5 | Tokens have no scopes — any valid token can mutate any VM | Issue one token per trusted caller; rotate on suspected leak | Tracked, post-M6 |
| G6 | No seccomp-BPF filter on the VMM process — a hypothetical KVM escape gets the full host syscall surface | Run inside a tightly-scoped systemd unit with `SystemCallFilter=` | Track E5 |
| G7 | `vm-kvm` is exercised by 1 test (under the `kvm` feature, NOT in CI) | Assume `vm-kvm` is **experimental** until M2 wires it into a real boot test | Tracked, M2 |
| G8 | No threat model for snapshot tampering — anyone with FS write access to the snapshot dir can swap state under a running pool | Run the VMM as the snapshot dir's only writer; consider signing manifests | Tracked, M5 |
| G9 | TLS is not terminated by the control plane | Terminate at your load balancer / sidecar — the binary listens on plain HTTP | Likely permanent; reverse-proxy is the right layer |
| G10 | Multi-replica rate limiting is single-process; sharding requires an external store | Single-replica deploys are unaffected | Follow-up once we have a prod multi-replica deployment |

## Out-of-scope (today)

- **Operator-side host compromise.** If the operator's machine is
  rooted, this project cannot help.
- **Cryptographic confidentiality of guest memory at rest.** The
  snapshot dir is plaintext. Encrypt at the FS layer.
- **DoS from a network adversary with no token.** Rate limiting
  protects per-token but not pre-auth (unauthenticated requests are
  rejected cheaply; sustained flooding is a load-balancer problem).
- **Side channels (Spectre, Rowhammer, etc.).** Mitigated by the
  host kernel — kept up-to-date by the operator.

## Disclosure

Found a vulnerability? File a **private** security advisory on the
GitHub repository (`Security → Report a vulnerability`). Do not
file a public issue. We aim to acknowledge within 72 hours and ship
a fix in the next milestone release. There is no bug-bounty program
pre-1.0.

## Change log

- **2026-05-16.** Initial draft. Reflects M0–M1 + M6 control-plane
  slice (auth, rate limiting, OpenAPI surface). Tracked gaps
  G1–G10. Bumps required as each milestone ships.
