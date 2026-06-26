# vmm-ipc — wire contract for per-VM process isolation

This crate defines the request/response shape and framing protocol
that the rust-nano-vm control plane uses to drive
`nanovm-vmm-child` worker processes. **It is the first PR in a
multi-PR arc toward production-grade per-VM cgroup isolation** — the
binaries that consume this contract land in subsequent PRs.

## Why per-VM process isolation

Today the control plane runs every VM inside a single VMM process
(see `crates/vm-kvm/src/cgroups.rs`). That makes the cgroups v2
caps **process-wide**: a runaway guest in tenant A's VM can starve
tenant B's VM inside the same VMM, because they share one
`memory.max`.

cgroups v2 only enforces *memory* accounting at the process level —
threaded cgroups can give per-thread CPU isolation but not per-thread
memory isolation. That's a kernel-design constraint, not something
we can engineer around. The right shape for real per-VM isolation
is the same one Firecracker uses: one VMM process per VM, each in
its own cgroup.

## Architecture (target shape)

```
                ┌───────────────────────────────┐
                │   nanovm-control-plane (host) │
                │   REST → orchestrator         │
                └────────────┬──────────────────┘
                             │
                             │  vmm-ipc over Unix socket
                             │  (length-prefixed JSON frames)
                             │
              ┌──────────────┼──────────────┐
              ▼              ▼              ▼
       ┌───────────┐  ┌───────────┐  ┌───────────┐
       │ jailer +  │  │ jailer +  │  │ jailer +  │
       │ vmm-child │  │ vmm-child │  │ vmm-child │
       │  (VM 1)   │  │  (VM 2)   │  │  (VM 3)   │
       │ in cgroup │  │ in cgroup │  │ in cgroup │
       └───────────┘  └───────────┘  └───────────┘

Each child VMM:
  - owns exactly one VM
  - runs in its own cgroup with per-VM memory.max + cpu.max
  - has its own seccomp filter (tighter than today's process-wide one)
  - dies in isolation on OOM, with no spillover to siblings
```

## Wire format

- **Framing.** 4-byte big-endian length prefix, then UTF-8 JSON
  payload. No delimiters, no escaping. Default cap is 4 MiB
  (`DEFAULT_MAX_FRAME_BYTES`) so a confused peer can't drive us into
  unbounded allocation. Configurable per-call via `read_frame_with_cap`.
- **Encoding.** `serde_json`. Same dep we already use across the
  workspace; round-trips every `vm-core` type without forcing a new
  serialization stack. JSON costs ~3 µs vs `bincode` per typical
  message — we'd revisit at >100k req/s, not before.
- **Discriminator.** `#[serde(tag = "kind")]` on both `Request` and
  `Response`. Wire looks like `{"kind":"start","id":42}` (easy to
  grep / `jq` in a captured pcap) rather than externally-tagged
  `{"start":{"id":42}}`.

## Roadmap — where this PR sits

| PR | Title | Status |
|---|---|---|
| **PR-1** | **`vmm-ipc` wire contract** (this PR) | **in flight** |
| PR-2 | `nanovm-vmm-child` worker binary | next |
| PR-3 | `nanovm-jailer` + per-VM cgroup wiring | after PR-2 |
| PR-4 | `process-fleet` `Hypervisor` impl in control plane | after PR-3 |
| PR-5 | pre-warmed VMM-process pool (preserves ~12 ms cold start) | after PR-4 |
| PR-6 | flip the default + delete the in-process path | final |

## What's intentionally NOT in this PR

- No binary changes. The crate ships its types ahead of the
  consumers so the protocol can be reviewed in isolation before any
  binary code locks the wire shape in.
- No streaming exec. `Request::ExecInGuest` is request/response; the
  streaming variant (matching the existing
  `Hypervisor::exec_in_guest_stream`) arrives once the basic
  lifecycle path is proven over the new transport.
- No async-stream multiplexing. One in-flight request per
  connection. Multiplexing arrives if-and-when the orchestrator
  proves it needs to pipeline.

## Cold-start tradeoff (honest disclosure)

Moving from in-process to one-VMM-process-per-VM adds the cost of
`fork()` + `execve()` + IPC handshake. On a typical x86_64 Linux
host that's ~5–15 ms on top of the existing ~12 ms KVM restore.
**PR-5's pre-warmed VMM-process pool** is the mitigation: keep N
idle `nanovm-vmm-child`s spun up and waiting, hand one a snapshot
when a customer fork arrives. Same shape as the existing in-process
warm pool, just at a different layer.

Realistic numbers we're targeting once PR-5 lands:

| Phase | p50 cold | p50 warm-pool |
|---|---|---|
| Today (in-process) | ~12 ms | ~1 ms |
| Mid-migration (PR-2…PR-4) | ~25 ms | n/a |
| Post-PR-5 | ~25 ms (first VM) | **~12-14 ms** (steady state) |

The README and bench will be updated in PR-6 to reflect the
shipped numbers — no marketing what the prototype can't deliver.
