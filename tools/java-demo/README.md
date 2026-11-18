# Reproducible Java production demo

`run.sh` builds and exercises the production-shaped Spring Boot path:

1. build the pinned JVM-capable Linux kernel;
2. build the pinned Oracle JDK + Spring Petclinic initramfs;
3. build `nanovm-jailer`, the KVM/smoltcp VMM child, and the demo
   binary;
4. boot one VM through `ProcessFleet`;
5. wait for the guest's readiness HTTP endpoint, exercise Petclinic
   over a second smoltcp port proxy, and snapshot the warmed VM;
6. restore multiple isolated fleet workers from that snapshot;
7. verify readiness and a real `/vets.html` response for every fork,
   then report restore, time-to-ready, and first-byte latency.

## Prerequisites

- Linux x86_64 with `/dev/kvm`;
- delegated cgroup v2 `memory` / `cpu` controllers when limits are
  configured (the demo itself requests no caps);
- Docker with `buildx`;
- Rust, `cpio`, `gzip`, and the Linux kernel build toolchain.

Run the complete flow:

```sh
tools/java-demo/run.sh --forks 3
```

Reusable kernel/rootfs build artifacts stay in their existing
`tools/*/cache` directories. VM sockets, per-worker snapshot stores,
workers, snapshots, and temporary proxy listeners are removed on exit.
Set `NANOVM_DEMO_SOCKET_DIR` to use an operator-managed socket
directory; the VM-owned entries are still cleaned by the fleet.

Useful overrides are forwarded to `nanovm-java-demo`, for example:

```sh
tools/java-demo/run.sh --forks 10 --warmup-timeout-secs 300
```

Set `NANOVM_SERIAL_STDERR=1` when diagnosing a guest boot; the worker
will mirror the captured serial stream to stderr without changing the
snapshot or HTTP lifecycle.

This stage does not provide a full Kubernetes operator or a
database-specific integration. Petclinic uses its existing in-guest
H2 fixture; the snapshot/fork primitives remain database-agnostic.
