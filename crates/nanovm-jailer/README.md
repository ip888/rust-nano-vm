# `nanovm-jailer`

Per-VM cgroup v2 setup + `execve()`. The privileged shim that runs once per VM, creates a fresh `nanovm-vm-<id>/` cgroup under the parent (with the requested `memory.max` and `cpu.max` written), attaches itself, then `exec()`s into `nanovm-vmm-child`. From the kernel's point of view the resulting process — and every subprocess it spawns — is in a single per-VM cgroup with hard caps. A fork-bomb inside the guest, an OOM allocator loop, a busy-spin vCPU thread: they all trip the per-VM cap and the rest of the host stays up.

This is **PR-3 of 6** in the per-VM cgroup isolation arc:

| PR | Crate | Status | What it does |
|----|----------------------|--------|---------------------------------------------------|
| 1  | `vmm-ipc`            | ✅ merged | Wire contract for the control-plane↔worker IPC.  |
| 2  | `nanovm-vmm-child`   | ✅ merged | Single-VM worker binary the jailer execs into.   |
| 3  | `nanovm-jailer`      | **this**  | Per-VM cgroup setup + execve.                    |
| 4  | process-fleet `Hypervisor` impl | pending | Control plane spawns one jailer per `create_vm`. |
| 5  | pre-warmed VMM-process pool     | pending | Spawn N jailers ahead of demand for sub-ms fork.  |
| 6  | flip default + delete in-process path | pending | Make the process-fleet backend the default.       |

## Architecture

```
         systemd unit
       Delegate=memory cpu
              │
              ▼
   /sys/fs/cgroup/<parent>/                ← parent cgroup
              │  (operator-managed)
              ▼
   ┌─────────────────────────────┐
   │ nanovm-control-plane spawns │
   │   nanovm-jailer per VM      │
   └──────────────┬──────────────┘
                  │ exec(&["nanovm-jailer",
                  │       "--vm-id", "42",
                  │       "--memory-limit-mib", "256",
                  │       "--cpu-quota-pct", "100",
                  │       "--vmm-child-binary", "/usr/local/bin/nanovm-vmm-child",
                  │       "--socket", "/var/run/nanovm/vm-42.sock"])
                  ▼
   ┌─────────────────────────────┐
   │ jailer (in parent cgroup)   │
   │  1. resolve parent          │
   │  2. check controllers       │
   │  3. create child cgroup     │  → /sys/fs/cgroup/<parent>/nanovm-vm-42/
   │  4. write memory.max=...    │
   │  5. write cpu.max=...       │
   │  6. write cgroup.procs=$$   │
   │  7. execve(worker)          │
   └──────────────┬──────────────┘
                  │ (process replaced; in child cgroup)
                  ▼
   ┌─────────────────────────────┐
   │ nanovm-vmm-child            │
   │  binds /var/run/nanovm/...  │
   │  serves IPC, runs the VM    │
   │  EVERY child it spawns      │
   │  inherits the cgroup        │
   └─────────────────────────────┘
```

## Quick start

### Requirements

- Linux with cgroup v2 unified hierarchy at `/sys/fs/cgroup` (every modern distro).
- A parent cgroup that has the `memory` and `cpu` controllers in its `cgroup.subtree_control`. The easiest way to get one is a systemd unit with `Delegate=memory cpu` (recommended), or root + `systemctl set-property` to delegate to the current slice.

### Build

```sh
cargo build --release -p nanovm-jailer
```

The `nanovm-jailer` binary lands at `target/release/nanovm-jailer`. The library half is also published as `nanovm-jailer` for callers that want to drive isolation programmatically — see the `JailerConfig` / `apply_isolation_and_exec` re-exports in `lib.rs`.

### Manual smoke test

The fastest way to convince yourself the jailer actually applies caps is to point it at a no-op stand-in for the worker and read the kernel-recorded values back:

```sh
# 1. Create a no-op stub the jailer will exec into.
cat > /tmp/stub.sh <<'EOF'
#!/bin/sh
echo "stub PID $$ in cgroup:"
awk -F: '/^0::/ { print $3 }' /proc/self/cgroup
sleep 60
EOF
chmod +x /tmp/stub.sh

# 2. Run the jailer under a systemd-run scope with the controllers
#    delegated (the only reliable way to get delegation in a shell).
systemd-run --user --scope --slice=nanovm.slice \
  -p Delegate=memory,cpu \
  target/release/nanovm-jailer \
    --vm-id 9001 \
    --memory-limit-mib 64 \
    --cpu-quota-pct 25 \
    --vmm-child-binary /tmp/stub.sh \
    --socket /tmp/nanovm-vm-9001.sock &

# 3. Verify the kernel-recorded caps.
PARENT=$(awk -F: '/^0::/ { print $3 }' /proc/self/cgroup)
CGROUP=/sys/fs/cgroup${PARENT}/nanovm-vm-9001
cat ${CGROUP}/memory.max  # → 67108864       (64 MiB exact)
cat ${CGROUP}/cpu.max     # → 25000 100000   (25% of one CPU)
cat ${CGROUP}/cgroup.procs

# 4. Tear down.
sudo rmdir ${CGROUP}
```

### Demo: fork-bomb stays contained

```sh
# Same setup, but worker is a fork-bomb instead of sleep.
cat > /tmp/fbomb.sh <<'EOF'
#!/bin/sh
:(){ : | : & }; :
EOF
chmod +x /tmp/fbomb.sh

systemd-run --user --scope --slice=nanovm.slice \
  -p Delegate=memory,cpu \
  target/release/nanovm-jailer \
    --vm-id 9002 \
    --memory-limit-mib 16 \
    --cpu-quota-pct 5 \
    --vmm-child-binary /tmp/fbomb.sh \
    --socket /tmp/nanovm-vm-9002.sock
# The fork-bomb runs INSIDE nanovm-vm-9002. The kernel kicks
# processes off once memory.max is hit; cpu.max throttles them
# to ~5% of one CPU. The rest of the host stays responsive.
```

## CLI

```
nanovm-jailer --help

USAGE:
    nanovm-jailer [OPTIONS] --vm-id <VM_ID> --vmm-child-binary <PATH> --socket <PATH>

OPTIONS:
        --vm-id <VM_ID>                   Numeric VM id (used to name the child cgroup)
        --memory-limit-mib <MIB>          Per-VM memory cap (MiB)
        --cpu-quota-pct <PCT>             Per-VM CPU quota in percent-of-one-CPU
        --socket <PATH>                   Unix socket the worker binds; passed through as `--socket`
        --vmm-child-binary <PATH>         Absolute path to nanovm-vmm-child
        --cgroup-parent <PATH>            Override the parent cgroup (default: own cgroup)
    -h, --help                            Print help
```

## What happens on failure

Every pre-exec failure is a distinct `JailerError` variant with an actionable diagnostic. The orchestrator (PR-4) maps each to either a 5xx (operator-fixable) or a 400 (caller-fixable). Examples:

| Failure                              | `JailerError` variant       | What to fix                                          |
|--------------------------------------|-----------------------------|------------------------------------------------------|
| Host doesn't have cgroup v2          | `NoCgroupV2`                | Boot the host with `systemd.unified_cgroup_hierarchy=1` (default since systemd 232). |
| Parent has no `memory` / `cpu` delegation | `ControllersMissing`     | `systemctl edit nanovm.service` → `Delegate=memory cpu`, then `daemon-reload` + restart. |
| `/sys/fs/cgroup/.../nanovm-vm-42` already exists | `AlreadyExists`     | A crashed predecessor or a double-spawn. `rmdir` the leftover and retry. |
| `cgroup.procs` write rejected        | `Io { path: "...cgroup.procs", ... }` | Wrong uid/gid; the parent's delegation didn't include this user. |
| `execve()` returns                   | `Exec { binary, source }`   | Worker binary doesn't exist / wrong arch / missing dynamic lib. |

## Tests

- **9 unit tests** in `src/lib.rs` cover the pure parsers: `parse_own_cgroup_line`, `child_cgroup_path`, `check_controllers`, `required_controllers`. No `/sys/fs/cgroup` needed.
- **1 end-to-end integration test** in `tests/end_to_end.rs` spawns the real jailer binary, points it at a shell stand-in for the worker, and asserts the kernel-recorded `memory.max` + `cpu.max` match the request. **Auto-skips** when the host lacks cgroup v2 / delegation / write access; prints a single `eprintln!` line explaining which precondition failed.

## License

Dual-licensed under Apache-2.0 OR MIT, matching the workspace.
