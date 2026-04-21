# Architecture

## Layering

```
┌───────────────────────────────────────────────────────────────┐
│  Clients                                                      │
│  ────────────────────────────────────────────────────         │
│  nanovm CLI  •  control-plane (axum REST / gRPC)              │
└───────────────────────┬───────────────────────────────────────┘
                        │ agent-sandbox-proto (serde / JSON-RPC)
                        ▼
┌───────────────────────────────────────────────────────────────┐
│  vm-core :: trait Hypervisor                                  │
│  ────────────────────────────────────────────────────         │
│  create_vm / start / stop / state / snapshot /                │
│  restore / destroy                                            │
└───────────────────────┬───────────────────────────────────────┘
                        │
      ┌─────────────────┴─────────────────┐
      ▼                                   ▼
┌──────────────────┐              ┌────────────────────────┐
│  vm-kvm (real)   │              │  vm-mock (tests, CI)   │
│  kvm-ioctls      │              │  in-memory HashMap     │
│  vm-memory       │              │  state machine only    │
│  linux-loader    │              │  no /dev/kvm required  │
└────────┬─────────┘              └────────────────────────┘
         │
         ▼
┌───────────────────────────────────────────────────────────────┐
│  Device model (virtio)                                        │
│  virtio-vsock  │  virtio-fs  │  snapshot (userfaultfd + CoW)  │
└───────────────────────┬───────────────────────────────────────┘
                        │
                        ▼
┌───────────────────────────────────────────────────────────────┐
│  guest-agent (static x86_64-unknown-linux-musl)               │
│  exec • fs • signals • stdio streaming                        │
└───────────────────────────────────────────────────────────────┘
```

## Core trait

```rust
pub trait Hypervisor: Send + Sync {
    fn create_vm(&self, cfg: &VmConfig) -> VmResult<VmHandle>;
    fn start(&self, id: VmId) -> VmResult<()>;
    fn stop(&self, id: VmId) -> VmResult<()>;
    fn state(&self, id: VmId) -> VmResult<VmState>;
    fn snapshot(&self, id: VmId) -> VmResult<SnapshotId>;
    fn restore(&self, snap: SnapshotId) -> VmResult<VmHandle>;
    fn destroy(&self, id: VmId) -> VmResult<()>;
}
```

All concrete backends live behind this trait. Callers (CLI, control plane,
benchmark harnesses) never name `vm-kvm` directly; they accept
`Arc<dyn Hypervisor>` or a generic `H: Hypervisor`.

## Why two backends

- **`vm-mock`** keeps CI green on any Linux runner, on any non-Linux
  developer laptop, and inside gVisor-style sandboxes that don't expose
  `/dev/kvm`. It also lets us unit-test control-plane state machines
  without boot time.
- **`vm-kvm`** is the real-world path. Heavy dependencies (`kvm-ioctls`,
  `vm-memory`, `linux-loader`) sit behind a non-default `kvm` cargo
  feature so a plain `cargo build --workspace` doesn't require them.

## Protocol boundary (`proto` crate)

Host ↔ guest-agent talks `agent-sandbox-proto`:

```
Request  { version, id, body: Ping | Exec | WriteFile | ReadFile | Stat | Signal }
Response { version, id, result: Ok(ResponseBody) | Err(RpcError { code, message }) }
```

Versioned at the envelope so the host can refuse a mismatched guest. Tag
names (`op`, `kind`) are part of the wire contract — the `proto` crate's
tests pin them.

## Memory / snapshot model (M5)

A base image is booted once, language runtimes loaded (Python + uv + Node),
and the VM is quiesced. Guest memory is captured via `userfaultfd` into a
CoW backing file. Every subsequent fork maps the backing file private, so
the first touched page pays a minor-fault cost and the unchanged pages
remain shared across siblings.

That pattern — "snapshot once, fork many" — is what lets agent eval
pipelines run 1000 variants cheaply. It is also the main differentiator vs
Firecracker (which has snapshot/restore, but not a native fork pool) and vs
E2B (which layers its own solution on top).

## Security posture

- Mirror the Firecracker threat model (documented there thoroughly).
- Seccomp-BPF filter on the VMM process (M1+).
- `cargo-fuzz` on virtio queue parsers before shipping real devices (M2+).
- `cargo-audit` / `cargo-deny` in CI (M0).
- No network to the guest unless explicitly requested; vsock only by
  default.
