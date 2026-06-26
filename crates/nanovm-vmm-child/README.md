# nanovm-vmm-child — single-VM VMM worker

The worker binary for the per-VM cgroup isolation arc. Listens on
a Unix socket the control-plane orchestrator picks for it, accepts
exactly one connection, and dispatches every incoming
[`vmm_ipc::Request`] to an in-process hypervisor backend.

This crate is **PR-2 of the arc.** PR-1 shipped the wire contract;
PR-3 ships the jailer that creates the per-VM cgroup before
`execve()`ing into this binary; PR-4 ships the orchestrator that
fleet-manages these workers from the control plane.

## What this binary does today

- Binds a `UnixListener` to the path passed via `--socket`. Removes
  any leftover file at that path first so a crashed predecessor
  doesn't block startup.
- Accepts one connection. The serve loop is single-conn-single-VM:
  pipelined requests are serviced in order, but a second
  orchestrator connecting concurrently would have to wait (it
  shouldn't — the orchestrator owns the 1:1 mapping).
- Dispatches each request through the standard `vm-core::Hypervisor`
  trait. The backend is `MockHypervisor` today; PR-3+ wires
  `KvmHypervisor` behind the same trait.
- Translates `vm-core::VmError` into `vmm_ipc::Response::Error`
  envelopes with stable `ErrorCode`s, using the
  `From<&VmError> for Response` impl shipped in PR-1. The host
  rebuilds the typed error from the wire.
- Exits cleanly on `Request::Shutdown`, on peer disconnect (EOF
  while reading the next frame), or on Ctrl-C / SIGTERM. Exits
  non-zero on a malformed frame so the operator can tell transport
  corruption apart from a clean stop.

## Usage

```sh
nanovm-vmm-child --socket /var/run/nanovm/vm-7.sock
```

`RUST_LOG=debug` to see each request/response pair; `RUST_LOG=warn`
in production. Logs go to stderr; stdout is reserved for future
out-of-band streams (e.g. exec output chunks) and is unused today.

## Architecture (target shape, repeated from the arc)

```
                ┌───────────────────────────────┐
                │   nanovm-control-plane (host) │
                │   REST → orchestrator         │
                └────────────┬──────────────────┘
                             │
                             │  vmm-ipc over Unix socket
                             │
              ┌──────────────┼──────────────┐
              ▼              ▼              ▼
       ┌───────────┐  ┌───────────┐  ┌───────────┐
       │ jailer +  │  │ jailer +  │  │ jailer +  │
       │ vmm-child │  │ vmm-child │  │ vmm-child │
       └───────────┘  └───────────┘  └───────────┘
```

Each `vmm-child` is one VM. PR-3's jailer creates the per-VM
cgroup, applies seccomp, then `execve()`s into this binary so the
worker starts already capped. PR-4's orchestrator manages the
fleet: spawns workers on demand, proxies REST calls to the right
worker's socket, reaps dead workers.

## Tests

- **5 unit tests** (`src/lib.rs`) over the pure `dispatch` function,
  driven against a `MockHypervisor` — every request kind round-trips
  to the typed response shape, including the `UnknownVm` and
  `InvalidTransition` error mappings.
- **8 integration tests** (`tests/integration.rs`) that spawn the
  built binary as a subprocess, connect over a real Unix socket,
  and exercise: ping, full lifecycle, unknown-vm error envelope,
  clean exit on peer disconnect, malformed frame surface,
  pipelined requests, split length-prefix-then-payload framing,
  and pre-accept signal handling.

## What's intentionally NOT here

- **No `--config` file.** PR-3's jailer will read the VM config out
  of `/proc/self/environ` or pass it on argv; the worker today
  takes its config inline via `Request::CreateVm`. Both shapes
  compose; we're keeping this binary minimal until the jailer
  arrives.
- **No streaming exec.** `Request::ExecInGuest` is request/response.
  The streaming wire shape lands once the request/response shape
  is proven through the orchestrator (PR-4 onward).
- **No real-KVM backend.** The single-VM `KvmHypervisor` is a
  bigger refactor of `vm-kvm`; that lands together with PR-3 so
  the jailer + per-VM cgroup + KVM all flip on at once.
