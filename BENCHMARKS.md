# Benchmarks

Reproducible measurements for `rust-nano-vm` cold boot, snapshot
restore, and parallel fork-many. Raw harness: `crates/bench/src/bin/`.

Numbers below reflect two passes:

- **Initial run (2026-10-14):** 10 cold / 10 restore / 50 parallel
  forks on a Broadwell laptop.
- **Multi-run refresh (2026-11-10):** `--runs 100` cold + restore for
  variance statistics; `--runs N` parallel-fork harness landed in the
  same pass (numbers TBD on next run).

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
written. With `--runs M`, the whole boot → snapshot → fork-N →
teardown cycle repeats M times and the harness prints across-run
variance (min / p50 / mean / p99 / stddev).

## Results

### 1. Cold boot — 100 runs (`--mode cold --runs 100`)

```
min  380 ms   p50 381 ms   p99 ~430 ms   max ~440 ms   mean ~390 ms
```

(Exact numbers from the 2026-11-10 run; `cold.p50 = 381.3 ms`.)

**First-pass 10-run outlier (683 ms) did not reappear at scale.** It
was a host-side TSC calibration miss that dropped the guest clock to
`refined-jiffies`. With 100 samples the tail is far tighter.

### 2. Restore from snapshot — 100 runs (`--mode restore --runs 100`)

```
min    2.3 ms
p50    2.6 ms
mean   2.9 ms
p99    3.9 ms
max    7.5 ms
```

**Speed-up vs cold boot (p50): 147.6×.** Snapshot creation itself is
a separate one-time ~50 ms cost.

The 100-run pass **tightened p99 from 7.4 ms (10-run sample) to
3.9 ms** — that 7.4 was a single-outlier artifact, not a persistent
tail. Typical restore cost is 2–3 ms with a ~2 ms spread; rare max
values at 7.5 ms (likely host scheduler jitter) are a real but
infrequent tail.

### 3. 50 parallel forks from one snapshot — single run

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

Multi-run variance for this benchmark is available via
`--forks 50 --runs 10`; the across-run block prints min / p50 /
mean / p99 / stddev for total wall-clock, per-fork p50 / p99, and
speed-up. Expected: total wall-clock stddev ~ ±5 % on this
single-core host, dominated by scheduler jitter between runs.

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
4. **Variance from 100-run restore.** p99 at 3.9 ms is from one 100-
   run session. A different host / kernel / power-state (laptop
   battery vs AC) could shift the tail. Re-run the harness locally
   to recheck on your own hardware.

## Reproduce

```sh
# One-time: build the agent initramfs (needs musl Rust target).
rustup target add x86_64-unknown-linux-musl
NANOVM_INIT=agent tools/initramfs/build-initramfs.sh

# Cold + restore benchmark, 100 runs each for tight variance.
cargo run -p bench --release --features kvm --bin nanovm-boot-bench -- \
    --mode both --runs 100

# Parallel fork-many — single 50-fork run.
cargo run -p bench --release --features kvm --bin nanovm-parallel-fork-bench -- \
    --forks 50 --verbose

# Parallel fork-many — multi-run across-run variance (recommended).
cargo run -p bench --release --features kvm --bin nanovm-parallel-fork-bench -- \
    --forks 50 --runs 10
```

Both binaries print the summary blocks shown above. Multi-run output
shows a compact per-run line + an aggregate table with min / p50 /
mean / p99 / stddev of total wall-clock, per-fork p50, p99, and
speed-up.
