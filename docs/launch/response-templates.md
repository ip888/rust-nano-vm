# Response templates for the first week

Copy these. Edit lightly to fit the specific person/comment. Reply
within 1 hour on launch day, within 24 hours after that.

The categories cover ~80% of what you'll be asked.

---

## A. Common HN / Reddit / Twitter questions

### "How is this different from Firecracker?"

> Firecracker is a general-purpose serverless VMM with
> snapshot/restore (~125 ms). It doesn't have a native fork primitive
> — every restore allocates fresh anonymous memory for the guest RAM.
> rust-nano-vm's snapshot is page-aligned on disk and
> `mmap(MAP_PRIVATE)`-ed on restore, so a 1000-way fan-out shares the
> read-only golden image via the page cache. Different workload —
> agent eval, not FaaS — different primitive.
>
> The fork-many code is here:
> https://github.com/ip888/rust-nano-vm/blob/main/crates/vm-kvm/src/lib.rs#L1888

### "12 ms cold start sounds impossible — what about the kernel boot?"

> No kernel boot. The snapshot captures a post-boot guest. The 12 ms
> is the time from "POST /v1/snapshots/:id/fork" to "vCPU runs first
> guest instruction". The snapshot is captured once, ahead of time;
> fork is `mmap` + restore vCPU/LAPIC/PIT state + KVM_RUN.

### "What about security?"

> Same isolation as Firecracker — KVM hardware boundary, no shared
> kernel with the host. The snapshot file has a header magic +
> version + page-count consistency check, validated before mmap. No
> userfaultfd, no in-VM kernel modules, no shared memory beyond what
> KVM_SET_USER_MEMORY_REGION sets up.

### "Why not use Firecracker as a library?"

> Firecracker is a binary, not a library — its public API surface is
> HTTP, not Rust. To get a snapshot-fork primitive we'd need to fork
> Firecracker and add it. Building on `kvm-ioctls` directly is ~5x
> less code (~3500 lines total) and lets us own the on-disk format.

### "Is this production-ready?"

> Pre-1.0. KVM data plane is real and the numbers are measured. M3
> (virtio-fs) is a scaffold. Control plane has bearer auth, per-token
> quota, metering, and Prometheus, but no TLS — put nginx in front.
> I use it as a personal eval harness; I wouldn't run untrusted code
> from the public internet on it yet.

### "Have you considered Wasmtime / WASM?"

> WASM is great when the guest can be recompiled to it. AI agents
> emit Python / Node / shell / arbitrary binaries — most of that
> can't run in WASM without major effort. KVM lets the guest be a
> real Linux distro that runs anything.

### "Have you considered gVisor / runsc?"

> gVisor's user-space kernel is great when you control the workload
> and can tolerate the syscall-emulation cost. For agent eval, cold
> start + memory density matter more than instruction-throughput
> overhead, and the hardware boundary is simpler to reason about.

### "Why is the per-fork Pss only 0.5 MiB? That seems too low."

> The trick is in the metric — Pss (proportional set size) divides
> each shared page's size by the number of processes mapping it. The
> ~7 MiB golden image is mapped by N forks via MAP_PRIVATE; the
> kernel serves the same physical page until a fork writes to it, so
> the Pss per fork is `golden_size/N + per_fork_dirty`. At N=50 that
> works out to ~0.5 MiB. RSS would double-count and report ~7 MiB
> per fork — that's the figure most benchmarks (wrongly) quote.

### "Does it work on macOS / Windows?"

> No. KVM is Linux-only. Bench + control-plane + mock backend work
> anywhere `cargo build` works, so you can develop on Mac/Windows and
> test against the mock; the real KVM path needs Linux with /dev/kvm.

### "Will you make a managed/hosted version?"

> Eventually, probably. Right now I'm focused on the open-source
> primitive being correct and fast. If you want managed hosting, drop
> your email in a GitHub issue and I'll reach out when there's
> something to try.
>
> (^ this is a soft lead-magnet — works because anyone who self-asks
> for "managed hosting" is qualified.)

---

## B. Bug reports

### Reproducible bug

> Thanks for the repro. Filed as #NNN:
> https://github.com/ip888/rust-nano-vm/issues/NNN
>
> I'll look at it this evening. Subscribe to the issue for updates.

### Unreproducible bug

> Sorry you're hitting this. To help me track it down, can you share:
> - `uname -a`
> - `cargo --version` and `rustc --version`
> - The full command you ran
> - The full output (paste in a code block or attach a file)
>
> If you can reproduce in <100 lines, paste them in too. I'll open an
> issue once I have enough to investigate.

### "It crashed!"

> KVM crashes are almost always one of: missing /dev/kvm permissions,
> nested virtualization disabled in BIOS, or running inside a
> container without `--device /dev/kvm`. Can you run
> `ls -la /dev/kvm` and paste the output?

---

## C. Feature requests

### Aligned with roadmap (containerd, k8s, multi-tenant, etc.)

> Yes, this is on the roadmap — see
> [docs/PLAN.md](https://github.com/ip888/rust-nano-vm/blob/main/docs/PLAN.md).
> Tracking in #NNN. If you want to send a PR, I'll review; otherwise
> I'll get to it after the next milestone.

### Out of scope (Windows guests, GPU passthrough, etc.)

> Thanks for the suggestion — this is out of scope for the core, at
> least until 1.0. The project is intentionally narrow: Linux guests,
> agent-eval workloads, ~Mb of memory per guest. If you need
> `<feature>`, Firecracker / Cloud Hypervisor / Kata are better
> primitives for that shape.
>
> Closing as `wontfix` — not because the idea is bad, but because
> shipping a focused thing matters more right now. Happy to chat in
> the issue if you want to push back.

### Genuinely intriguing but unclear

> Interesting — can you say more about the use case? I want to
> understand if this is a small change that solves a real problem, or
> a large change that solves a niche problem. (Either is fine; the
> answer changes the priority.)

---

## D. Cold emails from recruiters / companies

### Generic recruiter

> Thanks for reaching out. I'm building rust-nano-vm full-time as a
> nights/weekends project on top of my main job. Open to chatting if
> the role is directly related to systems Rust / virtualization / AI
> infra — not interested in adjacent Rust roles (web/backend) for now.
>
> If it fits, send me a one-page summary of the role and I'll reply
> within 48 hours.

### AI infra company

> Thanks — I've seen your work on `<thing they shipped>`. Happy to
> chat. A 30-minute video call works; here's my Calendly:
> `<your-link>`. Specifically interested in: what your current
> sandbox primitive looks like, what cold-start latency you're seeing
> at p99, and where rust-nano-vm fits (or doesn't) in your stack.

### Acquisition / acqui-hire feeler

> Appreciate it. The project is too early for an acquisition
> conversation, but I'm open to chatting about what a working
> relationship would look like — consulting on your sandbox layer, a
> commercial license for closed-source distribution, contributor
> agreements, etc.
>
> 30-minute call to scope it? Calendly: `<your-link>`

---

## E. Press / blog inquiries

### Tech journalist asking for a piece

> Happy to do an interview. Can you share:
> - The publication
> - The angle (technical deep-dive? AI-infra trends? OSS-monetization?)
> - The format (written interview? video? quoted in a roundup?)
> - The deadline
>
> Once I have those I can decide if it's a fit for the project at
> this stage. Some specifics I won't comment on yet: revenue,
> customers, future commercial plans.

### Conference talk invitation

> Thanks for thinking of me. Realistically I can do 1–2 talks per
> year, virtual preferred. If the talk is on systems Rust / KVM /
> serverless internals at a venue that gets the audience right, I'm
> interested. Send me the CFP + audience profile and I'll reply
> within 1 week.

---

## F. "Will you do this for free?" — politely declining unpaid work

> The project is open source; the *time* isn't. For ad-hoc support /
> custom integration / private-fork maintenance I do paid consulting.
> Day rate is `<your-rate>`. Happy to scope a small engagement (e.g.
> a one-day deep dive into integrating it with your stack) if that
> works for you.
>
> If you just want to file a bug or a feature request, the GitHub
> issue tracker is the right channel and that's always free.

(Set your day rate ahead of time. Don't improvise on first ask.
Solid starting point for solo Rust systems work in 2026: $1500–2500/day
EU, $2000–3500/day US — adjust to your timezone & seniority.)

---

## Things to NEVER reply

- "lol", "imo", "tbh" — keep it professional, you're under a microscope on launch day
- "I disagree" without explanation
- Anything that complains about HN/Reddit voters
- "I'll add that" without an issue + timeline
- Revenue numbers, customer names, NDA'd info
- Anyone's name without their permission (no name-dropping in public)

---

## How to use this file

On launch day, open it in a tab. When a question comes in:

1. Find the closest match (A.x / B.x / etc.)
2. Copy the template
3. Edit the specifics for *this* commenter (name them, reference their exact words)
4. Send
5. **Add a `→ template A.3 used Mon 9:15a` note** so you can spot when you're hitting the same question 5x and need to update the README / FAQ instead.

By end of week 1, you'll have a pretty good sense of which 5 questions
get asked the most. Those become `docs/faq.md` — *that's* what reduces
your reply burden long-term.
