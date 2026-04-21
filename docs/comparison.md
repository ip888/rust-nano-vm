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
| Device model | virtio-block, virtio-net, vsock | Same + virtio-fs (M3) for agent file exchange |
| Control plane | Out-of-scope (bring your own) | Included (`control-plane` crate, M6) |
| Guest agent | Out-of-scope | Included (`guest-agent`, M2, static musl) |

Firecracker remains the right choice for general serverless. `rust-nano-vm`
only wins for the agent workload.

## vs E2B

E2B is the closest commercial incumbent.

| Axis | E2B | rust-nano-vm |
| --- | --- | --- |
| Language | Go + Firecracker | All-Rust |
| License | Proprietary managed service | Apache-2.0 / MIT dual |
| Cold start | 150–400 ms | < 50 ms target |
| Snapshot + fork | Layered on top of Firecracker | Native in `snapshot` crate (M5) |
| Self-host | Limited | First-class; single binary |
| Protocol | Proprietary SDK | Open `agent-sandbox-proto` spec |
| Pricing | Managed per-second | OSS free; optional managed cloud |

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

- **Need fastest possible cold start for agent eval?** rust-nano-vm.
- **General serverless function platform?** Firecracker.
- **Managed service, no ops?** E2B.
- **Shared-kernel OK, want the simplest thing?** Docker.
- **On-prem healthcare / finance / defense?** rust-nano-vm (self-host) or
  Kata.
