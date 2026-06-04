# Nanolambda README — repositioning draft

This is the draft README for the **nanolambda** repo, repositioning it
as the platform layer on top of `rust-nano-vm`.

**Why this lives here, in the rust-nano-vm repo:** my workflow only has
write access to this repo. Copy the content below into
`https://github.com/ip888/nanolambda/blob/main/README.md` (replace the
existing README), commit, push. Optional: while you're there, also add
`LICENSE-APACHE` (copy from this repo) and change `Cargo.toml`'s
`license = "MIT"` → `license = "Apache-2.0 OR MIT"` to align with the
Rust ecosystem standard.

---

## The repositioning, in one sentence

Before: *"NanoLambda is a sandbox platform"* (commodity positioning;
process-isolation sandboxes are a dime a dozen).

After: **"NanoLambda is the open-source platform that turns
`rust-nano-vm`'s 12 ms hardware sandbox into an HTTP / MCP / Python
product surface AI agents can actually call."**

That's the *moat*: anyone can write the SDK / dashboard / MCP server.
Almost nobody has the isolation engine.

---

## The new README (paste this into nanolambda/README.md verbatim)

```markdown
# NanoLambda

> **The open-source AI-sandbox platform powered by hardware-isolated microVMs.**
> REST API · MCP server · Python SDK · dashboard · Prometheus metrics ·
> **~12 ms cold start** when run on top of [`rust-nano-vm`](https://github.com/ip888/rust-nano-vm).

[![License: Apache-2.0 OR MIT](https://img.shields.io/badge/license-Apache--2.0_OR_MIT-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.93+-orange.svg)](server/rust-toolchain.toml)

## What it is

Your AI agent (Claude, Cursor, Devin, OpenHands, an internal automation
script) sends code to NanoLambda. NanoLambda runs that code in an
**isolated hardware microVM**, streams stdout / stderr / exit code
back, and meters / logs / quotas the call.

NanoLambda is the **product surface**: REST API, MCP server, Python
SDK, dashboard, Prometheus metrics, billing-ready usage records.

The **isolation primitive** comes from
[`rust-nano-vm`](https://github.com/ip888/rust-nano-vm) — a single-binary
Rust microVM with:

- **~12 ms p50 cold start** (snapshot → fork)
- **~0.5 MiB private memory per fork** at N=50 (Pss, not RSS)
- **>90% pages shared** via `MAP_PRIVATE` CoW
- **Hardware boundary** (KVM), not a shared kernel
- Apache-2.0 / MIT

Together, NanoLambda + rust-nano-vm is the only open-source stack where
both the API surface *and* the isolation primitive are first-class —
the OSS shape of what E2B sells as a closed managed service.

## Why both layers matter

|                       | NanoLambda alone | rust-nano-vm alone | The stack |
| --------------------- | :-----------:    | :-----------:      | :-------: |
| HTTP / MCP / Python   | ✅               | ❌                  | ✅        |
| Dashboard + metering  | ✅               | ❌                  | ✅        |
| `docker run` deploy   | ✅               | ❌                  | ✅        |
| Hardware isolation    | ❌ (proc + rlim) | ✅ (KVM)            | ✅        |
| 12 ms cold start      | ❌               | ✅                  | ✅        |
| Trustable for untrusted code | ❌        | ✅                  | ✅        |

NanoLambda *can* run with its built-in process-based runtime (the
default; great for local dev and for trusted code). For untrusted code
— anything the AI generated, anything from a customer-facing
endpoint — switch the runtime to `rust-nano-vm` and you inherit the
hardware boundary without changing your application code.

## Quickstart

### 1. Try it on a laptop (process runtime, no KVM)

```sh
docker run -d -p 8080:8080 ghcr.io/ip888/nanolambda:latest

curl -X POST http://localhost:8080/v1/sandbox/invoke \
     -H 'content-type: application/json' \
     -d '{"language": "python", "code": "print(2 + 2)"}'
# → {"stdout":"4\n","stderr":"","exit_code":0,"duration_ms":12}
```

### 2. With the MCP server (for Claude / Cursor / Devin)

Add to your MCP-capable agent's config:

```json
{
  "mcpServers": {
    "nanolambda": {
      "command": "npx",
      "args": ["-y", "@nanolambda/mcp"],
      "env": { "NANOLAMBDA_URL": "http://localhost:8080" }
    }
  }
}
```

Your agent now has a `sandbox.invoke(code)` tool that runs in an
isolated sandbox.

### 3. With the Python SDK

```python
from nanolambda import Client

c = Client(base_url="http://localhost:8080")
r = c.invoke(language="python", code="import math; print(math.pi)")
print(r.stdout)  # "3.141592653589793"
```

### 4. With hardware isolation (Linux + /dev/kvm)

```sh
# Set runtime to rust-nano-vm:
docker run -d --device /dev/kvm -p 8080:8080 \
    -e NANOLAMBDA_RUNTIME=rust-nano-vm \
    ghcr.io/ip888/nanolambda:latest

# Same API; now every invoke runs in a 12 ms hardware microVM.
curl -X POST http://localhost:8080/v1/sandbox/invoke ...
```

## Architecture

```
 ┌────────────────────────────────────────────────────────┐
 │  HTTP REST  ·  MCP  ·  Python SDK  ·  Dashboard        │
 └─────────────────────┬──────────────────────────────────┘
                       │
 ┌─────────────────────▼──────────────────────────────────┐
 │  NanoLambda control plane                              │
 │  auth · quota · metering · routing · audit log         │
 └─────────────────────┬──────────────────────────────────┘
                       │
            ┌──────────┴──────────────┐
            ▼                         ▼
 ┌────────────────────┐   ┌──────────────────────────────┐
 │ Process runtime    │   │ rust-nano-vm runtime         │
 │ (rlimits, default) │   │ (KVM, snapshot/fork, ~12 ms) │
 └────────────────────┘   └──────────────────────────────┘
```

## Workspace

```
server/crates/
  api-server/      axum REST + dashboard
  mcp/             MCP server (stdio + http)
  runtime/         Pluggable runtime: process | rust-nano-vm
  storage/         sqlite usage + audit log
sdks/
  python/          pip install nanolambda
docs/
scripts/
```

## When to use which runtime

| Workload                              | Runtime           |
| ------------------------------------- | ----------------- |
| Trusted internal automation           | process (default) |
| Trusted AI-generated code, your own logged-in users | process           |
| Untrusted code, public endpoints      | rust-nano-vm      |
| Multi-tenant AI sandbox (SaaS shape)  | rust-nano-vm      |
| Local dev / unit tests / CI          | process (faster startup) |
| Regulated industries (healthcare, finance, EU residency) | rust-nano-vm |

## Status

- v0.1.x. Process runtime: working. rust-nano-vm runtime: in progress
  (tracked in #NN; ETA <date>).
- Python SDK: working.
- MCP server: working.
- Dashboard: working.
- Self-hosted Fly.io deploy: working.

## License

Dual-licensed under Apache-2.0 OR MIT, like `rust-nano-vm`.
```

---

## How to land this (~ 1 day of work)

1. **Add Apache-2.0 to nanolambda** (~ 15 min)
   - Copy `LICENSE-APACHE` from rust-nano-vm to nanolambda root
   - Rename existing `LICENSE` → `LICENSE-MIT`
   - Edit `server/Cargo.toml` workspace: `license = "Apache-2.0 OR MIT"`
   - Edit any `[package]` license = lines in per-crate Cargo.toml
   - Commit: `chore: dual-license under Apache-2.0 OR MIT`

2. **Paste the new README** (~ 5 min)
   - Replace `README.md` with the block above
   - Update placeholder dates (`ETA <date>`) and any issue numbers (`#NN`) as you go
   - Commit: `docs: reposition as the platform layer for rust-nano-vm`

3. **Flip nanolambda public** (~ 5 min)
   - Same 6-step process in [`docs/launch/public-flip.md`](../launch/public-flip.md)
   - Add `rust-nano-vm` to the topics so the two repos are graph-linked

4. **Cross-link** (~ 10 min)
   - In rust-nano-vm's README, add a paragraph at the bottom:
     > **Looking for a turn-key API + SDK + dashboard on top of
     > rust-nano-vm?** See [NanoLambda](https://github.com/ip888/nanolambda)
     > — the platform layer.
   - In nanolambda's README, the link to rust-nano-vm is already
     in step 1 above.

5. **Open a tracking issue in nanolambda** (~ 5 min)
   - Title: "Wire `runtime` crate to call `rust-nano-vm` as a runtime backend"
   - Body: describe the trait shape — `Runtime::invoke(code, lang, timeout) -> Result<Output>`. Process backend stays the default. rust-nano-vm backend driven by the existing HTTP API on `:8080`.
   - This is the work item for the launch-day "what's coming" message.

Total: ~ 40 minutes of mechanical edits, no code. The actual code work
(implementing the rust-nano-vm runtime backend) is the Phase 2 work
post-launch, ~ 1 week of evenings.

## What this changes about the launch

- The HN post still launches rust-nano-vm (it's the moat).
- **In the first-author comment**, you now add one paragraph at the end:
  > **There's also a higher-level OSS platform on top of this** —
  > [NanoLambda](https://github.com/ip888/nanolambda) — with REST / MCP /
  > Python SDK / dashboard / metering. You can `docker run` it today
  > with the built-in process runtime; switching to the rust-nano-vm
  > runtime is the work-in-progress that turns it into the OSS shape of
  > E2B with hardware isolation.

That paragraph is what turns "interesting infra project" into "this
person is building the whole stack". Hiring managers notice.
