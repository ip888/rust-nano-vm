# X / Twitter thread

Post within 30 minutes of the HN submission so the two amplify each
other. Quote-tweet the first tweet with the HN link once HN is up.

Each tweet is ≤280 chars (counted including spaces and the implicit
"n/N" thread numbering). Replies stay under the same author — don't
@-mention anyone in the thread itself.

---

## Tweet 1 (the hook)

```
I built a microVM in Rust with a ~12 ms cold start.

For AI-agent code execution: fan out 1000 sandboxes from one snapshot
in seconds, each costing ~0.5 MiB of host memory.

The whole "trick" is one mmap call. Thread 🧵 (1/7)
```

## Tweet 2 (the problem)

```
Every AI coding agent — Claude Code, Cursor, Devin, eval harnesses —
needs to run generated code somewhere safe.

The options today:

- E2B: 150–400 ms cold start, closed SaaS
- Firecracker: ~125 ms, no native fork
- Containers: weak isolation

None fit agent-eval. (2/7)
```

## Tweet 3 (the trick)

```
The shape of the answer: snapshot once, fork many.

Write the guest RAM out as a page-aligned file. On fork:
mmap(MAP_PRIVATE, fd, ...) → hand to KVM → restore vCPU state →
KVM_RUN.

Every fork shares the read-only pages of the golden image via the
kernel page cache. (3/7)
```

## Tweet 4 (the code)

```
Here's the whole "trick" — ~50 lines:

  MmapRegion::build(
      Some(file_offset),
      mem_len,
      PROT_READ | PROT_WRITE,
      MAP_NORESERVE | MAP_PRIVATE,  // ← this is the magic
  )

It's piggybacking on the same machinery Linux uses for fork(2) on
processes. (4/7)
```

## Tweet 5 (the numbers)

```
Measured on a stock i5 laptop, 8 GiB RAM, vanilla Linux:

• ~12 ms p50 cold start
• ~0.51 MiB per-fork Pss at N=50
• >90% pages shared via CoW
• ~30 000 concurrent forks per 16 GiB host (projection)

Per-fork memory DECREASES as fan-out grows. That's the CoW win. (5/7)
```

## Tweet 6 (the write-ups)

```
Two write-ups if you want the internals:

1. The cold-start primitive (why MAP_PRIVATE is everything):
   github.com/ip888/Rust-nano-vm/blob/main/docs/blog/01-mmap-private.md

2. Faithful KVM snapshot/restore in <1000 lines of Rust:
   github.com/ip888/Rust-nano-vm/blob/main/docs/blog/02-snapshot-restore.md

(6/7)
```

## Tweet 7 (the ask)

```
Repo: github.com/ip888/Rust-nano-vm
Apache-2.0 / MIT. Single binary, Rust, ~12 crates.

Solo project — if you're working on this problem (or hiring people who
work on it), I'd love to hear from you. 👋

(7/7)
```

## Quote-tweet (once the HN post is live)

```
Also on HN: <paste HN submission URL here>

If you have questions about the snapshot/fork internals, ask them
there or in the GitHub issues — I'll be around all day.
```

## Don't

- Don't tag people in the original thread. Tag in *replies* only, when
  you genuinely want their attention on a specific question they'd
  care about (e.g. tag a Firecracker maintainer on tweet 4 if you've
  worked with them before).
- Don't ask for retweets. The thread either works or it doesn't.
- Don't run the thread through ChatGPT for "more engagement" — Rust /
  systems Twitter sniffs it out in seconds.

## If a tweet gets traction

A single tweet from the thread will probably outperform the rest. If
tweet 5 (the numbers) is the breakout, reply to *it* with the GitHub
link, a screenshot of the bench output, or a one-line follow-up
("happy to answer specific questions about the measurement
methodology"). Don't make people scroll back to tweet 7.
