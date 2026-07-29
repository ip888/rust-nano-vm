# Show HN — post + top-comment defenses

## Title

```
Show HN: nanovm – sub-second KVM microVMs for AI-agent code execution
```

## URL

```
https://nanovm.example.com
```

## Text body

```
Hi HN. I've been building nanovm — a single-binary Rust microVM
that forks a real KVM sandbox in ~12 ms so LLM agents (Claude
Code / Cursor Agent / LangChain / OpenAI Assistants) can actually
run their tool calls somewhere safe without paying 100–500 ms per
call for a Docker cold start.

The core trick is snapshot + fork as a first-class primitive: one
prepared "golden" agent VM, snapshotted once, then each tool call
maps that snapshot's memory file MAP_PRIVATE and lets KVM restore
from it. ~50 lines of unsafe in vm-kvm/src/vmstate.rs is the
whole trick. Cold start disappears into a mmap.

There's a live 20-fork benchmark on the landing page (running
against a public demo tenant) so you can click before you sign
up. And there's an in-browser Python playground behind signup —
paste code, hit Run, watch it execute in a real fresh KVM
microVM.

Everything (VMM + guest agent + REST control plane + Python SDK +
TypeScript SDK + Helm chart) is Apache-2.0 OR MIT dual-licensed.
The hosted plans buy throughput + support + an SLA; self-host
stays free forever.

Would love feedback — especially from anyone who has hit the
"sandbox my agent's tool calls" problem and found the current
options (E2B, Modal Sandbox, Docker exec) either too slow, too
proprietary, or both.

Repo: https://github.com/ip888/rust-nano-vm
Numbers: https://nanovm.example.com/why-nanovm
Playground: https://nanovm.example.com/dashboard/playground
(free tier, no card)
```

## First-comment technical deep-dive (post yourself)

```
A few implementation notes for people who want the details:

- Every fork is a real KVM microVM boot from a snapshot, not a
  container. Each has its own kernel + rootfs, seccomp filter on
  the vmm process, cgroups on the jailer. Firecracker's threat
  model, one binary.
- The ~12 ms number is warm-pool p50 measured with our public
  fork-latency harness (`cargo run -p api-bench`) against the
  hosted control plane. p99 is ~28 ms; cold-restore (empty pool)
  is under 30 ms.
- The snapshot/restore path serialises vCPU + LAPIC + FPU + MSR +
  IRQCHIP + PIT state via kvm-bindings' serde feature. Guest RAM
  lives in a separate backing file so restore is a mmap, not a
  full memory copy. Write-up:
  https://nanovm.example.com/blog/02-snapshot-restore.

- The control plane is ~500 lines of axum: bearer auth, per-token
  + per-org token-bucket quota on /fork, Prometheus /metrics,
  OpenAPI 3.1 contract, dunning enforcement, RBAC on destructive
  routes. Enterprise: audit-log SIEM webhook + air-gap install
  docs + BYO snapshot store (S3 or filesystem).

- Python SDK on PyPI (`pip install nanovm`), TypeScript SDK on
  npm (`@nanovm/sdk`), OpenAI-shape tool descriptors work with
  LangChain.js's bindTools, Vercel AI SDK, and Anthropic tool
  use unchanged.

Comparison against E2B / Modal / AWS Lambda MicroVMs / Docker,
with cold-fork p50s side by side and honest recommendations to
use the other guy when they're the right pick:
https://nanovm.example.com/why-nanovm

Happy to answer specific questions.
```

## Prepared defenses — the four questions that WILL come up

### Q1 — "How is this different from Firecracker + your own control plane?"

```
Firecracker is what nanovm is built on conceptually — same threat
model (Rust VMM, minimal device model, seccomp/jailer). Three
practical differences:

1. Snapshot+fork is a first-class primitive here. Firecracker
   supports snapshot save/restore, but not native fork; each
   restore reloads guest memory. nanovm's fork is `mmap
   MAP_PRIVATE` on the snapshot's memory file, so the kernel
   serves the read-only golden pages to every child. ~0.5 MiB Pss
   per fork at N=50 concurrent, dropping as N grows.
2. The control plane, guest agent, and per-tenant billing are in
   the same binary. Firecracker leaves that to you.
3. Distribution — single Rust binary vs a VMM + jailer + your own
   agent + your own metering.

If you'd rather build it yourself on top of Firecracker, the
snapshot-fork technique is documented in blog post 01
(https://nanovm.example.com/blog/01-mmap-private). Take it.
```

### Q2 — "How is this different from E2B / Modal Sandbox?"

```
Cold start (p50): ~12 ms vs 150–400 ms (E2B) vs ~200 ms (Modal
Sandbox). The difference matters most for agent-eval fan-out —
an agent that makes 100 tool calls per task pays ~1.2 s of
sandbox overhead here vs 15–40 s on the managed alternatives.

License: nanovm is Apache-2.0 OR MIT and self-hostable in one
binary; E2B is a proprietary managed service; Modal is closed
source. If you want to run in your VPC, only nanovm supports
that today.

Pricing: nanovm hosted is $29/mo Pro (unlimited monthly forks)
vs E2B's per-second billing. For eval workloads the fixed cap
tends to be cheaper.

Full side-by-side with the two things they do better than us
(managed convenience, marketplace of pre-built envs):
https://nanovm.example.com/why-nanovm
```

### Q3 — "Why not just Docker / gVisor / containers?"

```
Fair question — for most workloads containers are the right
answer. Two places they specifically don't work well for LLM
agents:

1. Cold start. `docker run` is 50–200 ms depending on image
   layer count. A ReAct loop with 50 tool calls pays 2.5–10 s of
   overhead you don't have to. nanovm collapses that to <1 s
   total.
2. Threat model. Namespaces share a kernel with the process
   they're isolating. A kernel-level escape (rare, historic,
   real) crosses your trust boundary. For agent code the model
   sometimes emits nonsense that a malicious npm postinstall
   would exploit — the KVM boundary matters.

Detailed comparison including LangChain Sandbox (Pyodide+Deno,
which IS a great pick for pure-Python workloads):
https://nanovm.example.com/why-nanovm
```

### Q4 — "12 ms sounds too good; how are you measuring?"

```
Server-reported wall-clock inside the /v1/snapshots/:id/fork
handler, so the number excludes network RTT between the caller
and the control plane. Reproduce with:

  cargo run -p api-bench --release -- \
      --api-url https://api.nanovm.example.com \
      --token nv_YOUR_TOKEN \
      --marketplace-name python-3.12-minimal \
      --n 100 --warmup 10

That prints a markdown table with p50/p90/p95/p99/min/max/mean/
stddev + a text histogram. The landing page's LiveForkBenchmark
does the same thing from the browser against the public demo
tenant.

The `~12 ms` cite is warm-pool p50 on a Fly.io performance-2x.
Cold-restore (empty pool) is ~28 ms p50. Both are measured, not
projected.
```

## Response cadence

- **First 2 hours**: reply within 15 min to every comment. Even
  a one-line acknowledgement counts — it's the top-page dwell
  signal, not the reply length.
- **Hours 2–6**: reply within 30 min.
- **Hours 6–24**: hourly.
- **Beyond 24 h**: daily.

Don't engage below-comment scores unless the same question comes
up three times (then swap in the prepared answer verbatim).
