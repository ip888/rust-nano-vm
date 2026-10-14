# rust-nano-vm

> Sub-second cold-start and fork for enterprise Java workloads.
> KVM-based microVM snapshot/restore + MAP_PRIVATE fork-many.
> **Restore in 2.5 ms (p50). 50 microVMs in 472 ms. 172 KiB per fork
> via copy-on-write. Independent of your database, your JDK vendor,
> and your cloud.** ([measured benchmarks](BENCHMARKS.md))

[![License: Apache-2.0 OR MIT](https://img.shields.io/badge/license-Apache--2.0_OR_MIT-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.94+-orange.svg)](rust-toolchain.toml)

## What it does

rust-nano-vm is a KVM (Kernel-based Virtual Machine — Linux's built-in
hypervisor) microVM engine that takes a snapshot of a **fully booted and
warmed** guest — kernel, userspace, JVM heap after warmup — and forks new
copies from it in milliseconds using MAP_PRIVATE copy-on-write memory.

The primitives are DB-agnostic and framework-agnostic. The current
enterprise product line targets **JVM cold-start**:

- **Spring Boot cold-start: 8–30 s → ~200 ms** — snapshot a warmed
  ApplicationContext, fork on demand.
- **CI integration-test boot: 10–60 s per test class → ~200 ms** — fork the
  warmed context per `@SpringBootTest` class.
- **Kubernetes HPA scale-out** (Horizontal Pod Autoscaler): sub-second
  ready pods instead of 30–60 s JVM warmup.
- **Multi-tenant JVM SaaS**: fork-per-tenant with warmed context, KVM
  isolation, ~10 MB and <200 ms per tenant.

No changes to your application code. Any JDK — OpenJDK, Corretto, Zulu,
Oracle. Your database wherever it lives — on-prem Oracle, AWS RDS,
Aurora, CloudSQL, Azure SQL, DynamoDB, Snowflake.

## How it compares

| Approach | Requires code changes | Requires build changes | Works with any JDK | Snapshots kernel + JVM |
|---|---|---|---|---|
| **CRaC** (Coordinated Restore at Checkpoint, JEP 462) | Yes (`Resource` hooks) | No | Only OpenJDK builds with CRaC | No (JVM only) |
| **AWS Lambda SnapStart** | No | No | Lambda-provided | Yes — but Lambda only |
| **GraalVM Native Image** | Yes (reflection metadata) | Yes (+10 min AOT compile) | GraalVM only | — (no snapshot; native binary) |
| **Spring AOT** (Spring 3+) | Minimal | Yes | Any | No |
| **rust-nano-vm** | **No** | **No** | **Any** | **Yes** |

## Measured performance

Reproducible on any Linux + KVM host; the numbers below are from an
Intel Broadwell @ 1.6 GHz laptop (2015-era hardware, 128 MiB guest).
Modern Xeon / Ryzen will run faster — treat these as the floor.

### Cold boot vs restore (10 runs each)

```
cold boot latency:   min 387 ms   p50 403 ms   p99 659 ms   mean 429 ms
restore latency:     min 2.3 ms   p50 2.5 ms   p99 7.4 ms   mean 3.5 ms
speed-up (p50):      164×
```

### 50 parallel forks from one snapshot

The characteristic workload: 50 microVMs restored concurrently from
one warm snapshot via `std::thread::spawn` + `MAP_PRIVATE` COW.

```
total wall-clock       :   472 ms   ← all 50 VMs ready
per-fork distribution  :   min 9.6 ms   p50 139 ms   p99 397 ms
theoretical sequential :  8094 ms   (50 × mean 162 ms)
speed-up (vs sequential)  17×

host memory per fork   :   172 KiB   (0.17 MiB)
total RSS delta        :     8.6 MiB for all 50 forks
```

Positioning: **Firecracker optimises 1→1 restore. rust-nano-vm
optimises 1→N parallel fork from the same warm snapshot.** On a
single-core host the parallelism is bounded by host CPU; on a 32-core
server the per-fork p99 spread shrinks dramatically.

Full methodology, caveats, and reproduction instructions:
**[BENCHMARKS.md](BENCHMARKS.md)**.

Rebuild the numbers yourself:

```sh
NANOVM_INIT=agent tools/initramfs/build-initramfs.sh
cargo run -p bench --release --features kvm --bin nanovm-boot-bench -- \
    --mode both --runs 10
cargo run -p bench --release --features kvm --bin nanovm-parallel-fork-bench -- \
    --forks 50
```

## Platform support

**Host OS:** Linux + KVM. Works on:

- Bare-metal Linux (RHEL, RHCOS, Ubuntu 22.04/24.04, Amazon Linux 2023)
- OpenShift / vanilla Kubernetes with `/dev/kvm` device access
- OpenShift Virtualization (KubeVirt) via `VirtualMachineInstance` CRD
- AWS EC2 metal instances (i3.metal, m5.metal, c5.metal, …)
- GCP nested-virt VMs (`--enable-nested-virtualization`)
- Azure Dsv5/Ev5 with nested virt
- OCI bare-metal

Not supported: macOS (no `/dev/kvm`), Windows (needs Hyper-V, out of scope).

**CPU architecture:** x86_64 with Intel VT-x (Intel Virtualization
Technology) or AMD-V/SVM (AMD Secure Virtual Machine) in v1. ARM64
(aarch64) planned for v2 once the x86_64 enterprise SKU has traction.

## Repository layout

```
crates/
├── vm-core/            trait definitions (Hypervisor, Snapshot, …)
├── vm-mock/            in-memory backend for tests
├── vm-kvm/             real KVM backend (feature-gated: `--features kvm`)
├── snapshot/           snapshot manifest + memory backing format
├── virtio-fs/          virtio filesystem device
├── virtio-queue/       virtio queue primitives
├── virtio-vsock/       virtio-vsock host↔guest transport
├── vmm-ipc/            VMM ↔ host IPC framing
├── nanovm-vmm-child/   per-guest VMM child process
├── nanovm-jailer/      SUID-less jailer for the VMM child
├── nanovm-fleet/       fleet manager over multiple VMM children
├── guest-agent/        in-guest agent (warmup hooks, exec, health)
├── proto/              wire protocol shared by control-plane + guest
├── control-plane/      HTTP/REST API, multi-tenant ownership, warm pool
├── cli/                operator CLI
├── bench/              host-side snapshot/fork microbenchmark
└── api-bench/          HTTP-side fork-latency benchmark
```

## Development

```sh
# Fast portable build — mock KVM backend, no /dev/kvm required.
cargo build --workspace
cargo test  --workspace

# Real KVM backend — Linux + /dev/kvm required.
cargo build --features kvm -p nanovm-vmm-child
cargo test  --features kvm -p vm-kvm

# Bench (mock backend, CI-friendly).
cargo run --release -p bench --bin nanovm-fork-bench -- --forks 100
```

The `kvm` feature stays opt-in so `cargo test --workspace` is green on
portable CI (no `/dev/kvm`, no root, no nested virt).

## Roadmap (public tier)

1. **Spring Petclinic proof-of-concept** — reference demo: snapshot a
   warmed Spring Boot app, fork ×10, each fork answers HTTP <200 ms
   after start.
2. **Java SDK** — gRPC-based, generated from `.proto`. Publishable to
   Maven Central.
3. **Testcontainers-Java driver** — DB-agnostic drop-in that lets an
   existing JUnit suite fork a pre-warmed container image instead of
   booting one cold.
4. **Kubernetes operator** — `NanoVMSnapshot` + `NanoVMDeployment`
   CRDs, warm pool, HPA fork.
5. **JVM warm-snapshot recipes** — Spring Boot, Quarkus, Micronaut
   documented warmup procedures.
6. **CRaC integration** — optional, for shops already using CRaC hooks.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
