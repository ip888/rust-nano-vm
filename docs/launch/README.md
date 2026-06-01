# Launch playbook

Copy-paste-ready material to launch `rust-nano-vm` publicly. The goal of
the launch is to get the project in front of:

1. AI-agent infra engineers (potential users, potential employers).
2. Rust systems-programming crowd (potential contributors, GitHub stars).
3. Hiring managers at companies that *need* this primitive (E2B, Modal,
   Replit, Together, regulated-industry MLOps).

## Targets

| Channel | File | Notes |
| --- | --- | --- |
| Hacker News (Show HN) | [`hn-show.md`](hn-show.md) | The big one. Title + first-author-comment. |
| `/r/rust` | [`reddit-rust.md`](reddit-rust.md) | Slightly more technical body. |
| X / Twitter | [`twitter-thread.md`](twitter-thread.md) | 7-tweet thread, can post same day as HN. |
| Lobste.rs | (skip unless you have an invite) | High-signal Rust/systems audience. |

## Timing

- **Post HN on a Tuesday, Wednesday, or Thursday between 08:00–10:00 ET.**
  Show HN front-page lifetime is ~6–8 hours on a good day. Posting
  earlier in US morning catches both EU afternoon and US morning. Avoid
  Friday/Saturday/Sunday (low volume, fewer front-page slots).
- **Post the X thread within 30 minutes of the HN submission.** Quote-tweet
  your own thread with the HN link once it's submitted, so the two
  amplify each other.
- **Post on `/r/rust` the same day, ~2 hours after HN.** Don't post to
  multiple subreddits — `/r/rust` is the only one where this lands.
  `/r/programming` is too broad; `/r/MachineLearning` doesn't accept
  infra posts.
- **Don't post to Lobste.rs unless you have an invite** — submitting from
  the link form without one looks spammy.

## The first hour matters

On HN, the upvote velocity in the first 60 minutes is what gets you onto
the front page. Two rules:

1. **Reply to every top-level comment in the first hour.** Even "thanks"
   is fine if there's nothing technical to say. Engagement signals to
   HN's ranker that the post is alive.
2. **Don't be defensive.** If someone says "this is just Firecracker
   with a different API", say "the snapshot-fork primitive isn't in
   Firecracker — here's the code: [link to vmstate.rs]". Don't argue.
   Don't downvote. Don't ask people to upvote — that gets the post
   killed.

## Response templates

Save these. You will get asked the same five questions.

**"How is this different from Firecracker?"**
> Firecracker is a general-purpose serverless VMM with snapshot/restore
> (~125 ms). It doesn't have a native fork primitive — every restore
> allocates fresh anonymous memory for the guest RAM. rust-nano-vm's
> snapshot is page-aligned on disk and `mmap(MAP_PRIVATE)`-ed on
> restore, so a 1000-way fan-out shares the read-only golden image via
> the page cache. Different workload — agent eval, not FaaS — different
> primitive.

**"Cold start in 12 ms is impossible / what about the kernel boot?"**
> No kernel boot — the snapshot captures a post-boot guest. The 12 ms is
> the time from "POST /v1/snapshots/:id/fork" to "vCPU runs first guest
> instruction". The snapshot is captured once, ahead of time; fork is
> just `mmap` + restore vCPU/LAPIC/PIT state + KVM_RUN.

**"What about security?"**
> Same isolation as Firecracker — KVM hardware boundary, no shared
> kernel with the host. The snapshot is a file with a header magic +
> version + page-count consistency check, validated before we mmap it.
> No userfaultfd, no in-VM kernel modules, no shared memory beyond what
> KVM_SET_USER_MEMORY_REGION sets up.

**"Why not use Firecracker as a library?"**
> Firecracker is a binary, not a library — its public API surface is
> HTTP, not Rust. To get a snapshot-fork primitive we'd need to fork
> Firecracker and add it. Building the right primitive on `kvm-ioctls`
> directly is ~5x less code (~3500 lines of Rust total) and lets us
> control the on-disk format.

**"Is this production-ready?"**
> Pre-1.0. The KVM data plane is real and the numbers are measured.
> M3 (virtio-fs) is a scaffold. The control plane has bearer auth,
> per-token quota, metering, and Prometheus, but no TLS terminator —
> put nginx in front. I'm using it as a personal eval harness; I
> wouldn't run untrusted code from the public internet on it yet.

## The line at the end of every post

> Apache-2.0 / MIT. Solo project; if you're working on this problem (or
> hiring people who work on it), I'd love to hear from you.

That's the line that turns a launch into job/contract conversations.
Don't skip it. Don't make it longer.

## Checklist for launch day

- [ ] Pull main locally; verify the bench reproduces (`cargo run -p bench --features kvm --release -- --count 100 --alive 50`)
- [ ] Open the README in GitHub web view and spot-check that the
      anchors in this playbook still resolve
- [ ] Open `docs/landing.html` in a browser and spot-check rendering
- [ ] Have GitHub notifications on (you want to see the first stars in
      real-time so you can reply to "I starred this because…" issues)
- [ ] Submit to HN (title from [`hn-show.md`](hn-show.md))
- [ ] Immediately paste the first-author-comment from [`hn-show.md`](hn-show.md)
- [ ] Post the X thread from [`twitter-thread.md`](twitter-thread.md)
- [ ] 2 hours later: post to `/r/rust` from [`reddit-rust.md`](reddit-rust.md)
- [ ] Reply to every top-level HN comment in the first hour
- [ ] After 24 hours: write down the top 3 questions you got; turn them
      into a `docs/faq.md` for next time

## After the launch

Three outcomes, three follow-ups:

1. **It hits the HN front page** (>200 points, >50 comments). Expect a
   wave of GitHub issues asking for containerd integration, language
   SDKs, hosted version. Triage hard — accept the issues that align
   with the existing roadmap, close the ones that don't with a kind
   note. **Phase 2 (containerd-shim) becomes the obvious next thing.**

2. **It does moderately** (50–200 points). You got eyeballs but not
   inbound. Spend a week on Phase 2 anyway, then re-launch with a
   "containerd integration" angle.

3. **It bombs** (<50 points). The story didn't land. Don't relaunch
   the same content. Take a week off, then write one *new* technical
   post (e.g., "measuring CoW: why Pss not RSS") and post that.
   Restart the conversation from a fresh angle.

The repo and the numbers are good; the only variable is whether the
right people see them. This playbook maximises that probability with
the minimum amount of outbound effort.
