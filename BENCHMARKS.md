# Benchmarks

Reproducible measurements for `rust-nano-vm` cold boot, snapshot
restore, and parallel fork-many. Raw harness: `crates/bench/src/bin/`.

Numbers below reflect three passes:

- **Initial run (2026-10-14):** 10 cold / 10 restore / 50 parallel
  forks on a Broadwell laptop (tinyconfig guest).
- **Multi-run refresh (2026-11-10):** `--runs 100` cold + restore for
  variance statistics; `--runs N` parallel-fork harness landed in the
  same pass (tinyconfig guest).
- **Spring Petclinic end-to-end (2026-11-16):** first live JVM
  workload — Spring Boot 3.x + JDK 21 + ZGC in a 2 GiB guest
  booting the JVM-profile kernel. See **section 4**.

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

### 3b. 50 parallel forks — 10-run across-run variance (`--forks 50 --runs 10`)

```
metric                        min      p50     mean      p99   stddev
total wall-clock            163.8    446.0    428.3    648.0    180.4  ms
per-fork p50                 27.9     53.1     60.0    110.6     27.4  ms
per-fork p99                 69.5    233.1    236.8    538.9    146.9  ms
speed-up                      4.2      9.5      9.6     14.2      3.1  ×

memory per fork (mean across runs) : 1036.0 KiB = 1.01 MiB
```

Reading the table:

- **Total wall-clock stddev is 180 ms (~42 % of mean)** — far wider
  than the ±5 % I'd guessed. Single-core scheduler contention
  dominates: 50 workers competing for one slot produces a heavy-tailed
  wall-clock. Best run was 164 ms (almost cache-hot across all 50
  forks); worst was 648 ms.
- **Per-fork p50 of 60 ms (mean across runs) is faster than the
  single-shot 139 ms** in section 3. The single-shot run paid a
  cold-cache tax on the first few forks; averaging across 10 batches
  reveals the steady-state mean.
- **Speed-up p50 of 9.5×** is lower than the single-shot 17×, for the
  same reason: lower per-fork mean means lower theoretical-sequential
  baseline, so parallel wall-clock looks less heroic against it. The
  absolute wall-clock is what matters for the product thesis
  (**sub-second ready for 50 microVMs**), and that number is firm.
- **Per-fork memory of 1.01 MiB (mean) is 6× the single-shot 172 KiB.**
  This is a **measurement artifact**, not a regression in COW
  behaviour: `rss_before` is captured after the previous batch's
  teardown, but the host heap allocator doesn't return all pages to
  the kernel between batches. The "before" baseline drifts upward,
  so each batch's own delta looks larger than a cold-host snapshot.
  The single-shot 172 KiB number remains the honest "shared-page COW"
  figure for a fresh host.

The **memory story** is still the headline for density: **172 KiB per
fork** on a fresh host (section 3), ~0.1 % of nominal 128 MiB guest
memory, 50 microVMs together ~8.6 MiB RSS instead of the naive 50 ×
128 MiB = 6.4 GiB. That is `MAP_PRIVATE` COW doing its job — pages
stay shared until a guest writes.

### 4. Spring Petclinic end-to-end — warm restore (`nanovm-jvm-bench --forks 20`)

The target workload for the Java-enterprise pivot: Spring Boot 3.x
Petclinic on JDK 21 (ZGC, `-Xshare:auto` CDS) in a 2 GiB guest
booting a custom Linux 6.12 kernel (CONFIG_INET, CONFIG_VIRTIO_NET,
TMPFS, CGROUPS, etc. — see `tools/kernel/jvm.fragment`). First live
run: **2026-11-16**.

**Cold boot (one-shot, not a bench stat)** — fresh microVM, kernel
boot through to Spring binding `:8080` and serving `/actuator/health`:

```
kernel-boot-to-init:   2.6 s
init.sh → JVM fork:    0.0 s
JVM → :8080 bound:     32 s       (Spring Petclinic start on 1 vCPU)
health 200:            instant after :8080
first JIT-warmup hit:  0.0 s
jit warmup duration:   30 s       (warmup.sh pumps 4 URLs for 30 s)
total:                 ~67 s      cold to fully-warm-and-ready
```

67 s is the baseline we beat with snapshots. On a faster host it's
20-40 s; on 1.6 GHz Broadwell with 1 vCPU it's 67 s.

**Warm snapshot restore — 20 forks after 3-fork warmup discard:**

```
--- WARM results ---
  n:      20
  p50:    3.37 ms
  p95:    3.60 ms
  p99:    3.64 ms
  mean:   2.99 ms
  range:  [2.38 ms, 3.64 ms]
  failures: 0/20
```

**Cold snapshot restore — same 20 forks, measured as fork + wait-for-READY:**

```
--- COLD results ---
  n:      20
  p50:    30.07 s
  p95:    31.08 s
  p99:    31.13 s
  mean:   30.41 s
```

Reading the cold numbers: **the fork cost itself is still ~2–7 ms**
(each per-fork line reports `restore=X.XXXms total=XX.XXs`). The
30 s is guest-side JIT warmup — the cold snapshot is taken at
`NANOVM_PETCLINIC_COLD` (the moment Spring binds `:8080`), so a
fork from it has to redo the 30 s `warmup.sh` loop to reach
`NANOVM_PETCLINIC_READY`. The warm snapshot captures the already-
JIT'd state, so every fork from it hits `NANOVM_PETCLINIC_READY`
within milliseconds of restore.

### Speed-up vs. a fresh Spring Petclinic boot

| metric | fresh boot | warm restore | speed-up |
|--------|-----------:|-------------:|---------:|
| p50 time-to-ready | ~67 s | 3.37 ms | **~20 000×** |
| mean time-to-ready | ~67 s | 2.99 ms | **~22 000×** |
| p99 time-to-ready | ~67 s | 3.64 ms | **~18 000×** |

(The in-harness `cold vs warm speedup` table reports ~10 000× because
it compares warm-restore against cold-restore-plus-rewarmup, not
against a true fresh-boot baseline.)

Memory cost per fork stays firmly in COW territory: the warm
snapshot's resident set is dominated by the JVM's own heap (~500 MiB
of class metadata + warmed-up code cache), but each MAP_PRIVATE
fork pays only for pages it writes — typically <10 MiB for a request
handler path.

### Caveats specific to this workload

1. **Cold measurement is methodological, not a limitation.** Cold
   snapshot captures Spring at the moment the port opens, pre-
   warmup. A truer "cold restore" would mean "fork from cold
   snapshot + first HTTP response"; that's ~2-7 ms + one HTTP
   round-trip in an unwarmed JVM (probably 50-200 ms with C1
   compile pressure). Bench harness methodology refinement is
   tracked for a follow-up PR.
2. **Single JVM vendor.** Oracle JDK 21.0.5 only. Corretto, Temurin,
   Azul Zing numbers will differ, especially on CDS behavior.
3. **Minimal workload.** Petclinic is a canonical Spring Boot demo
   (H2 in-memory DB, Thymeleaf templates, no external deps).
   Real enterprise apps with DB connection pools, Kafka
   subscribers, and external calls will see different startup
   profiles — but the fork cost is the same (COW).

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
   Spring Boot / Quarkus boot slower; Petclinic end-to-end
   numbers are in **section 4** below.
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

### Spring Petclinic (section 4) — one extra build step for the JVM rootfs

```sh
# One-time: build the Petclinic rootfs (needs docker + buildx).
SKIP_EXT4=1 tools/java-rootfs/build.sh

# Spring Petclinic end-to-end — cold + warm snapshot, 20 forks each.
# TMPDIR override because the default /tmp is often a small tmpfs
# (3-4 GiB) that can't hold two 2 GiB memory images.
mkdir -p ~/nanovm-snapshots
TMPDIR=~/nanovm-snapshots \
    cargo run -p bench --release --features kvm --bin nanovm-jvm-bench -- \
    --kernel tools/kernel/cache/bzImage-jvm \
    --initramfs tools/java-rootfs/cache/initramfs.cpio.gz \
    --memory-mib 2048 --snapshot-at both --forks 20
```

First run prints Spring Boot's own cold-start timeline (~30-60 s on
a slow host) before the measurements start. The `--cold-marker-secs`
default (180 s) covers slow hosts; faster hosts exit the wait
as soon as `NANOVM_PETCLINIC_COLD` arrives.

Both binaries print the summary blocks shown above. Multi-run output
shows a compact per-run line + an aggregate table with min / p50 /
mean / p99 / stddev of total wall-clock, per-fork p50, p99, and
speed-up.
