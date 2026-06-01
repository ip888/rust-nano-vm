# Show HN submission

## Submission form

**Title** (max 80 chars, no emoji, no "I built", no caps lock):

```
Show HN: Rust-nano-vm – 12 ms cold-start microVMs for AI agents
```

**URL:**

```
https://github.com/ip888/Rust-nano-vm
```

Leave the "text" field on the submission form **empty**. HN frowns on
Show HN posts with both a URL and a text body; the convention is to put
your context as the first comment instead.

## First comment (paste this immediately after submitting)

```
Author here. I've been working on rust-nano-vm as a solo nights/weekends
project for the past few months. It's a single-binary Rust microVM aimed
at the niche I think is underserved: code execution for AI agents.

The numbers, all measured on an i5 laptop with 8 GiB of RAM and vanilla
Linux + KVM, no special hardware:

  - ~12 ms p50 cold start (snapshot → fork)
  - ~16 ms p99 (100 sequential forks of one snapshot)
  - ~0.5 MiB per-fork Pss at N=50 (>90% pages shared via MAP_PRIVATE CoW)
  - ~30 000 concurrent minimal-footprint forks per 16 GiB host (projection)

The whole "trick" is one mmap call. Snapshot writes the guest RAM out as
a page-aligned file. Fork mmap's that file MAP_PRIVATE | MAP_NORESERVE,
hands the region to KVM, restores vCPU + in-kernel-device state, and
KVM_RUN's. Every fork shares the read-only golden pages via the kernel
page cache; pages only diverge on first guest write. It's piggybacking
on the same machinery Linux uses for fork(2) on process address spaces.

The write-up of the cold-start primitive is here:
  https://github.com/ip888/Rust-nano-vm/blob/main/docs/blog/01-mmap-private.md

The snapshot/restore half (vCPU + LAPIC + PIT + MSRs + XSAVE, the state
you have to capture to actually resume instead of re-boot) is here:
  https://github.com/ip888/Rust-nano-vm/blob/main/docs/blog/02-snapshot-restore.md

Why I built it instead of using $existing: Firecracker is a great
general-purpose serverless VMM but doesn't have a native fork primitive
(~125 ms cold restore, fresh anonymous memory each time). E2B layers
fan-out on top of Firecracker but it's a closed managed service at
150–400 ms cold start with proprietary SDKs. Containers share a kernel
with the attacker's code. For agent eval (fan out 1000 variants of one
toolchain snapshot, run them for a few seconds, throw them away) none
of those shapes fit, and the obvious primitive doesn't exist as an
open building block.

There's a 30-second demo against a mock backend (no KVM needed) in the
README. On Linux with /dev/kvm you can reproduce the numbers with:

  cargo run -p bench --features kvm --release -- --count 100 --alive 50

Honest caveats: pre-1.0; virtio-fs is a scaffold; snapshots are
same-host / same-kernel (no live migration); the control plane has
bearer-token auth + per-token quota + Prometheus metrics but no TLS,
put it behind nginx if you expose it.

Stack: Rust, kvm-ioctls, vm-memory, axum, custom virtio-vsock. No
prometheus crate (hand-rolled exposition), no Firecracker fork. Apache-2.0
/ MIT.

Solo project; if you're working on this problem (or hiring people who
work on it), I'd love to hear from you. Repo:
https://github.com/ip888/Rust-nano-vm
```

## Why the title is this exact shape

- **"Show HN:"** prefix — required by HN convention for personal
  projects.
- **"Rust-nano-vm"** before "–" — gives the project a name people can
  remember and Google later. The dash format is HN-standard.
- **"12 ms cold-start"** — the killer number is in the title. HN scans
  fast; people will or won't click based on this number alone.
- **"microVMs"** (not "VMs") — the technical reader knows what this
  means and wants to know more. The non-technical reader scrolls past
  (good — they would have downvoted us in the comments).
- **"for AI agents"** — the niche. Avoids the "is this just Firecracker?"
  reflex by signalling the workload up front.

## What to do in the first hour

1. **Pin a tab on `hn.algolia.com`** searching for your submission URL
   so you see new comments instantly.
2. **Reply to every top-level comment.** Even short replies. Engagement
   is the ranker signal.
3. **Don't reply to obvious trolls.** Don't argue with "this is just X".
   Drop a code link and move on.
4. **Don't ask friends to upvote.** HN detects voting rings; the post
   gets flagged invisibly.
5. **If someone finds a real bug**, thank them, open the issue yourself,
   link it in your reply. Showing maintenance velocity in real-time on
   HN is *extremely* good optics.
