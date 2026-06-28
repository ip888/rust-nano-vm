# `nanovm-fleet`

Process-fleet [`Hypervisor`](https://docs.rs/vm-core) implementation. Spawns **one `nanovm-jailer` subprocess per VM**, the jailer creates a fresh cgroup v2 child with the requested `memory.max`/`cpu.max`, attaches itself, and `execve()`s into `nanovm-vmm-child`. Each VM gets its own crash domain: an OOM in one tenant's VM trips the kernel's cap and SIGKILLs *that* worker; the rest of the host keeps serving.

This is **PR-4 of 6** in the per-VM cgroup isolation arc:

| PR | Crate / change                                | Status     |
|----|------------------------------------------------|------------|
| 1  | `vmm-ipc` — wire contract                      | ✅ merged  |
| 2  | `nanovm-vmm-child` — single-VM worker          | ✅ merged  |
| 3  | `nanovm-jailer` — per-VM cgroup + execve       | ✅ merged  |
| 4  | `nanovm-fleet` — process-fleet `Hypervisor`    | **this**   |
| 5  | pre-warmed VMM-process pool + cross-worker restore | pending |
| 6  | flip default + delete in-process path          | pending    |

## Architecture

```
            nanovm-control-plane
                    │
                    │ ProcessFleet::create_vm(cfg)
                    ▼
            ┌──────────────────┐
            │ ProcessFleet     │
            │  • workers map   │
            │  • snapshot map  │
            │  • tokio runtime │
            └────────┬─────────┘
                     │ Command::spawn
                     ▼
        ┌─────────────────────────┐
        │ nanovm-jailer (per VM)  │      memory.max,
        │  setup cgroup, execve   │      cpu.max
        └────────────┬────────────┘
                     │ execve
                     ▼
        ┌─────────────────────────┐
        │ nanovm-vmm-child        │ ◀──────────────┐
        │   binds /var/run/...    │                │
        │   serves vmm-ipc loop   │                │ persistent
        └─────────────────────────┘                │ UnixStream
                                                   │ (one per VM)
                                                   │
            ProcessFleet routes Hypervisor methods ─┘
            over the persistent stream per VM.
```

## Quick start

```rust,no_run
use std::sync::Arc;
use std::path::PathBuf;
use nanovm_fleet::{FleetConfig, ProcessFleet};
use vm_core::{Hypervisor, VmConfig};

let fleet = ProcessFleet::new(FleetConfig {
    jailer_binary: PathBuf::from("/usr/local/bin/nanovm-jailer"),
    vmm_child_binary: PathBuf::from("/usr/local/bin/nanovm-vmm-child"),
    socket_dir: PathBuf::from("/var/run/nanovm"),
    default_memory_limit_mib: Some(256),
    default_cpu_quota_pct: Some(100),
    ..Default::default()
})?;
let fleet: Arc<dyn Hypervisor> = Arc::new(fleet);

let h = fleet.create_vm(&VmConfig::default())?;
fleet.start(h.id)?;
// ... use the hypervisor handle as you would the in-process backend
fleet.destroy(h.id)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

The control plane plugs `Arc<ProcessFleet>` into `AppState::new(...)` the same way it accepts `Arc<MockHypervisor>` or `Arc<KvmHypervisor>` today.

## Scope of PR-4

**Implemented:**
- `create_vm`, `start`, `stop`, `state`, `vm_meta`, `destroy`
- `snapshot`, `list_snapshots`, `delete_snapshot`, `snapshot_meta`
- `list_vms` — synthesizes from the fleet's worker map
- `exec_in_guest`, `read_file`, `write_file`
- Cooperative shutdown + SIGKILL-on-Drop for leaked workers

**Deliberately deferred:**

- `restore` — returns `VmError::Backend` with a message pointing to PR-5. Snapshots live inside the worker that captured them (single-VM workers + in-memory state); cross-worker restore needs either the durable snapshot store path or a shared-state refactor. Both land in PR-5 of the arc. The in-process `MockHypervisor` / `KvmHypervisor` paths still work for the snapshot/restore/fork workflow until PR-5 ships.
- `exec_in_guest_stream` — SSE-over-IPC needs its own framing addition to `vmm-ipc`. PR-5 / PR-6.
- `snapshot_export_dir` / `snapshot_adopt` — same wire-extension story as streaming exec.

## Sync ↔ async bridge

`Hypervisor` is sync; `vmm-ipc` is `tokio`. The fleet owns a dedicated multi-thread runtime (2 worker threads) and `block_on`s each IPC roundtrip. The control plane already wraps `spawn_blocking` around hypervisor calls in the routes layer, so blocking the caller for the duration of the IPC round-trip is the documented contract — no reactor stall.

## Tests

- **3 unit tests** in `src/lib.rs` — `FleetConfig::default` sanity, infallible construction, `worker_for` unknown-VM.
- **5 integration tests** in `tests/end_to_end.rs` — spawn the **real** `nanovm-vmm-child` binary against a shell-stub "jailer" (just `exec`s; no cgroups needed) and exercise the full orchestration loop including `Drop` cleanup. Runs on any Linux host without cgroup v2 delegation — the cgroup wiring itself is covered by `crates/nanovm-jailer/tests/end_to_end.rs`.

## License

Dual-licensed under Apache-2.0 OR MIT, matching the workspace.
