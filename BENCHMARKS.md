# Benchmarks

Reproducible measurements for `rust-nano-vm` cold boot, snapshot
restore, and parallel fork-many. Raw harness: `crates/bench/src/bin/`.
Numbers below are from a first-pass run on 2026-10-14.

## Hardware & environment

| Item | Value |
|------|-------|
| CPU | Intel Core Broadwell @ 1.6 GHz, 1 logical core available |
| Host kernel | Linux 6.12.0 |
| Guest | Linux 6.12.0 tinyconfig + `nanovm-agent` (static musl PID 1) |
| Guest memory | 128 MiB |

This is a 2015-era laptop — treat the numbers as the **floor**.
Modern Xeon / Ryzen will run faster, especially under parallelism.

## What's measured

**Cold boot** (`nanovm-boot-bench --mode cold`): clock starts at
`create_vm`, stops when `nanovm-agent: ready` appears on serial.
End-to-end: KVM create, memory alloc, bzImage + initramfs load,
kernel boot, userspace agent exec.

**Restore** (`nanovm-boot-bench --mode restore`): `hv.restore(snap)`
wall-clock on an in-memory snapshot handle. Each run destroys its VM
before the next starts — sequential latency floor, no CPU
contention between runs.

**Parallel fork-many** (`nanovm-parallel-fork-bench --forks N`): the
characteristic workload. Snapshot a golden VM, `std::thread::spawn`
N host threads each calling `hv.restore(snap)` concurrently via
`Arc<KvmHypervisor>`. Every fork lives on its own `MAP_PRIVATE`
copy-on-write (COW) mapping sharing pages with the golden until
written.

## Results

### 1. Cold boot — 10 runs

```
min  387 ms   p50 403 ms   p99 659 ms   max 683 ms   mean 429 ms
```

p99 outlier (run #3, 683 ms) is a host-side TSC calibration miss that
dropped the guest to the `refined-jiffies` clock — single-core
scheduling jitter at ≤1-per-10-runs rate.

### 2. Restore from snapshot — 10 runs

```
min  2.3 ms   p50 2.5 ms   p99 7.4 ms   max 7.4 ms   mean 3.5 ms
```

**Speed-up vs cold boot (p50): 164×.** Snapshot creation itself is a
separate one-time ~50 ms cost.

### 3. 50 parallel forks from one snapshot

```
total wall-clock       : 472 ms         ← all 50 VMs ready
per-fork distribution  :
    min  9.6 ms   p50 139 ms   p99 397 ms   max 412 ms   mean 162 ms

theoretical sequential : 8094 ms        (50 × mean 162 ms)
speed-up vs sequential :   17×

Δ host RSS             : +8.6 MiB       (per fork 172 KiB = 0.17 MiB)
```

The per-fork spread (9.6 → 412 ms) is **host CPU contention** on one
core: all 50 workers compete for one scheduler slot. On a 32-core
server this spread shrinks dramatically; total wall-clock stays
similar until forks saturate host cores.

The **memory story** is the headline for density: 172 KiB per fork
is ~0.1 % of nominal 128 MiB guest memory. All 50 microVMs together
cost ~8.6 MiB RSS, not the naive 50 × 128 MiB = 6.4 GiB. That is
`MAP_PRIVATE` COW doing its job — pages stay shared until a guest
writes.

## Positioning

> **Firecracker optimises 1→1 restore. rust-nano-vm optimises 1→N
> parallel fork from the same warm snapshot.**

Firecracker's own `LoadSnapshot` latency is in the 100+ ms range per
VM. 50 of them back-to-back is seconds, not a half-second. That is
the gap the parallel-fork benchmark closes.

## Caveats (honest)

1. **In-memory snapshot.** `hv.snapshot(golden)` references memory
   still in the host page cache; the restore path hits that cache.
   Cold-disk restore (filesystem snapshot read back) is a separate
   benchmark not run here.
2. **1-core host.** Fork parallelism is bounded by host CPU count.
   Numbers from a many-core server would show tighter per-fork
   distribution and better speed-up at larger N.
3. **Minimal guest.** 128 MiB with only `nanovm-agent` as PID 1.
   Spring Boot / Quarkus boot slower; the workload-specific
   enterprise cold-start number is still TODO.
4. **Single run.** One 10-run (or 50-fork) sample each. Multi-run
   variance estimates would strengthen the claims; `--runs 100` is
   the obvious next pass.

## Reproduce

```sh
# One-time: build the agent initramfs (needs musl Rust target).
rustup target add x86_64-unknown-linux-musl
NANOVM_INIT=agent tools/initramfs/build-initramfs.sh

# Cold + restore benchmark.
cargo run -p bench --release --features kvm --bin nanovm-boot-bench -- \
    --mode both --runs 10

# Parallel fork-many.
cargo run -p bench --release --features kvm --bin nanovm-parallel-fork-bench -- \
    --forks 50 --verbose
```

Both binaries print the summary blocks shown above.
