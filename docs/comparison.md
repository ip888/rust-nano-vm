# Comparison

## vs Firecracker

Firecracker is the microVM we're building on top of conceptually. Key
differences:

| Axis | Firecracker | rust-nano-vm |
| --- | --- | --- |
| Target workload | Serverless / function-as-a-service | AI agent code execution |
| Language | Rust | Rust |
| Distribution | VMM binary + jailer + API server | Single binary (`nanovm`) |
| Cold start | ~125 ms (snapshot restore) | < 50 ms target (warm pool + CoW fork) |
| Snapshot | Full save/restore; no native fork | **First-class snapshot + fork** |
| Device model | virtio-block, virtio-net, vsock | Same + virtio-fs for agent file exchange |
| Control plane | Out-of-scope (bring your own) | Included (`control-plane` crate, axum REST) |
| Guest agent | Out-of-scope | Included (`guest-agent`, static musl) |

Firecracker remains the right choice for general serverless. `rust-nano-vm`
only wins for the agent workload.

## vs E2B

E2B is the closest commercial incumbent.

| Axis | E2B | rust-nano-vm |
| --- | --- | --- |
| Language | Go + Firecracker | All-Rust |
| License | Proprietary managed service | Apache-2.0 / MIT dual |
| Cold start | 150–400 ms | < 50 ms target |
| Snapshot + fork | Layered on top of Firecracker | Native in `snapshot` crate |
| Self-host | Limited | First-class; single binary |
| Protocol | Proprietary SDK | Open `agent-sandbox-proto` spec |
| Pricing | Managed per-second | OSS free; optional managed cloud |

## vs LangChain Sandbox (Pyodide + Deno)

LangChain's [`langchain-sandbox`](https://github.com/langchain-ai/langchain-sandbox)
runs untrusted Python via **Pyodide** (CPython compiled to WebAssembly)
inside **Deno**'s permission-flag runtime. The blog post frames this as
"running untrusted code without a sandbox" — technically there IS a
sandbox (the WASM VM), but there's no VM/container to run: it's a
package you install into your host process.

This is a *very* smart choice for a specific slice of workloads, and
it eats the low end of the "give my agent a sandbox" market. Where it
stops is where a real microVM starts:

| Axis | LangChain Sandbox | rust-nano-vm |
| --- | --- | --- |
| Setup effort | `pip install langchain-sandbox` | Docker + `/dev/kvm` (or Fly.io) |
| Cold start | ~50-100 ms (WASM init) | ~12 ms (KVM `MAP_PRIVATE` fork) |
| Pure Python (stdlib + Pyodide-compat wheels) | ✅ great | ✅ |
| Native-code Python (`torch`, `playwright`, `opencv`, `scipy`, …) | ❌ Pyodide package set only | ✅ real CPython + `pip install` |
| Shell commands (`curl`, `git`, `apt`, `sh -c "…"`) | ❌ no shell | ✅ |
| Non-Python runtimes (Node.js, Go, Rust, R, Julia) | ❌ Python-only | ✅ |
| Long-running processes (Jupyter kernel, dev server, DB) | ❌ WASM is per-call | ✅ VM stays running |
| Persistent filesystem between tool calls | ❌ WASM memory dies with the call | ✅ real ext4 rootfs (when you keep a VM alive via `/v1/vms` — `/v1/sandbox/invoke` is fork→run→destroy) |
| Multi-tenant hardware-enforced isolation | ⚠️ WASM VMs share the host process | ✅ per-VM cgroups + seccomp |
| Forensic audit log (per privileged action) | ❌ not built-in | ✅ JSONL audit + Prometheus per-org meter |
| Syscall filter (seccomp-BPF) | ⚠️ Deno permissions ≠ real seccomp | ✅ shipped (`crates/vm-kvm/src/seccomp.rs`) |
| Regulated-industry audit story (HIPAA / PCI / SOC 2) | ⚠️ WASM viable but non-standard for auditors | ✅ real Linux VM, familiar to auditors |
| Cost at 100 tool-calls-per-task | Free (in-process) | Free local; ~$0.30/hr Fly.io |
| Distribution | Python package | Single Rust binary + optional Helm chart |

**Use LangChain Sandbox when:** the model only writes pure Python
against stdlib + numpy / pandas / scipy (Pyodide wheels exist for
these), you never need shell, and you're running one agent per host
(personal projects, single-tenant apps). It's genuinely faster and
simpler for that shape.

**Use rust-nano-vm when:** the model ever needs `pip install X`
against a non-Pyodide package, calls a shell command, spawns a
subprocess that must outlive the tool call, writes files that must
persist across calls, or you're running a multi-tenant SaaS that
needs hardware isolation between customers and a per-org billing
meter.

The two aren't mutually exclusive; a production agent can route
cheap Python-only calls to Pyodide and everything else to nanovm.

## vs containers (Docker / gVisor / runsc / Kata)

| Axis | Containers | rust-nano-vm |
| --- | --- | --- |
| Isolation | Shared kernel (Docker) / user-space kernel (gVisor) / VM (Kata) | VM (KVM) |
| Cold start | ~100 ms (Docker) – 500 ms+ (Kata) | < 50 ms target |
| Attack surface | Large (Linux kernel) | Small (VMM + virtio) |
| Fork 1000 variants | Slow / expensive | Cheap via CoW snapshot fork |

Containers remain the right default for most workloads. For agent code
execution where cold start matters and an adversarial model is warranted,
a microVM is the right primitive.

## When to choose what

- **Model only writes pure Python, never needs shell / `pip install` / a native package?** LangChain Sandbox (Pyodide + Deno).
- **Need fastest possible cold start for agent eval, or the model needs shell / native packages / a real filesystem?** rust-nano-vm.
- **General serverless function platform?** Firecracker.
- **Managed service, no ops?** E2B.
- **Shared-kernel OK, want the simplest thing?** Docker.
- **On-prem healthcare / finance / defense?** rust-nano-vm (self-host) or
  Kata.
