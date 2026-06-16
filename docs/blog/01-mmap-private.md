# How rust-nano-vm cold-starts in ~12 ms: `mmap(MAP_PRIVATE)` is the whole trick

> **TL;DR.** Don't boot a kernel — clone its memory.
> `mmap(MAP_PRIVATE, fd, ...)` on a snapshot file lets the kernel hand
> every fork the same read-only physical pages of the golden image, and
> only copy on first guest write. Measured on a laptop: **~12 ms p50
> cold start, ~0.5 MiB Pss per fork at N=50, >90% pages shared.**

## What "cold start" usually means

The reason serverless microVMs feel slow is that "cold start" means
*"boot a kernel, init a guest, wire devices, jump into userspace"*.
Firecracker amortises some of that with snapshot/restore (~125 ms). E2B
layers fan-out logic on top (~150–400 ms). Even snapshot-restore still
copies the guest RAM image into a freshly-allocated anonymous mapping.

For an AI-agent workload that fans 1000 sandboxes out of one Python
toolchain, that's three orders of magnitude too slow and the
memory bill grows linearly in N.

## The actual primitive

The trick is to do less, not more:

1. **Once:** boot the guest you want to use as the golden image. When
   it reaches the state you care about (Python imported, deps loaded,
   FS warm), pause it and write its RAM out to a file. Capture vCPU +
   in-kernel device state alongside (LAPIC, IOAPIC, PIT, KVM clock,
   MSRs, XSAVE — see [post #2](02-snapshot-restore.md)).

2. **Per fork:** `mmap(MAP_PRIVATE, MAP_NORESERVE, fd, ...)` on the RAM
   file, hand the resulting region to KVM as `KVM_SET_USER_MEMORY_REGION`,
   restore vCPU + machine state, and `KVM_RUN`.

That's it. **No boot. No memcpy of the golden image.** The kernel
serves every reading fork the *same* physical page out of the page
cache. Only when a fork writes to a page does the kernel `COW` it into
the fork's own anonymous memory.

## What it looks like in the code

The whole "trick" is ~50 lines in
[`crates/vm-kvm/src/lib.rs`](../../crates/vm-kvm/src/lib.rs):

```rust
/// Build a guest-memory view that lazily reads pages from `path` and
/// copies-on-write on the first guest store: `mmap(MAP_PRIVATE, fd, …)` on
/// the memory image. Multiple forks of the same snapshot share their
/// unmodified pages via the kernel's page cache and only diverge for
/// pages they actually dirty — the unit-economics win.
fn cow_guest_memory(path: &Path, mem_bytes: u64) -> VmResult<GuestMemoryMmap> {
    let file = File::open(path)?;
    // ... validate header ...
    let file_offset = FileOffset::new(file, snapshot::MEMORY_DATA_OFFSET);
    // PRIVATE so guest writes CoW into the fork's own anonymous pages.
    // NORESERVE matches vm-memory's anonymous default (no overcommit reservation).
    let region = MmapRegion::build(
        Some(file_offset),
        mem_len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_NORESERVE | libc::MAP_PRIVATE,
    )?;
    // ...
}
```

The mirror image at snapshot time is
[`dump_memory`](../../crates/vm-kvm/src/lib.rs):

```rust
/// Dump the whole guest RAM to a snapshot memory image: a
/// BackingFileHeader followed by zero padding to MEMORY_DATA_OFFSET (so
/// the page data is page-aligned in the file and can be
/// `mmap(MAP_PRIVATE)`-ed directly on restore — the foundation of
/// fork-many CoW).
fn dump_memory(guest_mem: &GuestMemoryMmap, mem_bytes: u64, path: &Path) -> VmResult<()> { ... }
```

The header is intentionally page-padded so that `MEMORY_DATA_OFFSET` is
a multiple of `PAGE_SIZE`. That's a load-bearing detail: `mmap`'s
file-offset argument must be page-aligned, and we want the *guest's*
page 0 to land on a real page boundary in the file so a single fault
brings in exactly one guest page.

## What the kernel does for you

When fork #2 of a snapshot reads from a page neither fork has written:

1. KVM stage-2 fault → host page fault.
2. Linux walks the VMA, sees a `MAP_PRIVATE` file mapping at this
   offset.
3. Looks up the page in the page cache — it's already there from fork
   #1's earlier read.
4. Maps the page read-only into fork #2's page table. **No copy. No
   I/O.**

When fork #2 writes:

1. Write protect fault.
2. Kernel allocates a new anonymous page, copies the original 4 KiB
   into it, swaps it into fork #2's page table.
3. Fork #1 is untouched.

This is *exactly* the same machinery Linux uses for `fork(2)` on
process address spaces. We're getting microVM fan-out for free by
piggybacking on it.

## The measurement that matters

The big mistake when benchmarking this is to look at RSS. RSS counts
the full size of every mapped page, *including* shared ones, *for
every process that maps them*. If three forks share a 7 MiB golden
image, RSS reports 21 MiB. The number you want is **Pss** (Proportional
Set Size), which divides each shared page's size by the number of
sharers, so the 7 MiB shows up as 7 MiB total across all three.

Linux exposes it at `/proc/self/smaps_rollup`:

```
Rss:               12345 kB
Pss:                3456 kB    <-- this one
Pss_Anon:           1234 kB
Pss_File:           2222 kB
```

`nanovm-fork-bench` reads `smaps_rollup` after each density run:

```rust
// crates/bench/src/main.rs
fn pss_kib() -> io::Result<u64> {
    let s = fs::read_to_string("/proc/self/smaps_rollup")?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("Pss:") {
            // "Pss:   3456 kB"
            return rest.split_whitespace().next()
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| io::Error::other("malformed Pss line"));
        }
    }
    Err(io::Error::other("no Pss line"))
}
```

## The numbers (laptop, KVM, vanilla Linux)

On an i5 laptop with 8 GiB RAM, after one warm-up:

| N (concurrent forks) | Per-fork Pss | Shared % |
| --- | --- | --- |
| 10 | 1.01 MiB | 86.0% |
| 20 | 0.67 MiB | 89.5% |
| 50 | 0.51 MiB | 91.4% |

Per-fork Pss *decreases* as N grows. That's not a bug — that's the
shape of the win. The golden image cost is amortised across more
sharers as the fan-out grows. Fork latency stays flat at ~12 ms p50.

Projection: ~30 000 concurrent minimal-footprint forks per 16 GiB host.

## Reproduce

```sh
# One-time: build kernel + initramfs (Linux + /dev/kvm required)
tools/kernel/build-tiny-kernel.sh
tools/initramfs/build-initramfs.sh

cargo run -p bench --features kvm --release --bin nanovm-fork-bench -- \
    --count 100 --alive 50 --settle-secs 2
```

## What this is *not*

- **It's not magic.** You still need a snapshot. Capturing one costs
  the boot you didn't have to pay later.
- **It's not cross-host.** The snapshot encodes same-kernel, same-CPU
  vCPU state. Cross-host migration is a different problem.
- **It's not free for write-heavy guests.** Per-fork Pss is dominated
  by the guest's dirty set. A guest that immediately overwrites all
  its memory loses the CoW advantage.

For agent eval, none of those caveats bite. You want fan-out on one
host, from one snapshot, for a few seconds of work.

---

Next post: [Faithful KVM snapshot/restore in <1000 lines of Rust](02-snapshot-restore.md).

Code: https://github.com/ip888/Rust-nano-vm
