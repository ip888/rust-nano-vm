# /r/rust submission

Post 2 hours after HN goes up. /r/rust is more technical and friendlier
than HN — you can be longer-form, link to actual code, and skip the
"author here" framing.

## Title (max 300 chars, but keep it short)

```
rust-nano-vm: a single-binary microVM with ~12 ms cold-start and ~0.5 MiB per-fork memory (KVM + MAP_PRIVATE CoW)
```

## Body (paste as the post body — /r/rust accepts markdown)

```
Hi r/rust. I've been working on **rust-nano-vm** as a solo
nights/weekends project — a single-binary Rust microVM aimed at AI
agent code execution. I just got the cold-start latency under 15 ms on
a laptop and wanted to share both the numbers and how it works.

**The numbers** (measured on i5 laptop, 8 GiB RAM, vanilla Linux + KVM):

| Metric | Value |
| --- | --- |
| Cold start, p50 | ~12 ms |
| Cold start, p99 | ~16 ms |
| Per-fork Pss at N=50 | ~0.51 MiB |
| Pages shared via CoW | >90% |
| Projection at 16 GiB | ~30 000 concurrent forks |

**The trick**: when you snapshot a guest, write its RAM out as a
page-aligned file. When you "fork" the snapshot to make a new guest,
`mmap(MAP_PRIVATE | MAP_NORESERVE, fd, ...)` that file, hand the region
to KVM via `KVM_SET_USER_MEMORY_REGION`, restore vCPU + LAPIC + PIT +
MSRs + XSAVE, `KVM_RUN`. The kernel serves every fork the same physical
pages of the golden image out of the page cache; pages only diverge on
first guest write. It's the same machinery Linux uses for `fork(2)` on
process address spaces, repurposed for guest RAM.

The whole "trick" is ~50 lines:
https://github.com/ip888/Rust-nano-vm/blob/main/crates/vm-kvm/src/lib.rs#L1888

Full write-up of the cold-start primitive:
https://github.com/ip888/Rust-nano-vm/blob/main/docs/blog/01-mmap-private.md

Snapshot/restore (vCPU + LAPIC + PIT + MSRs + XSAVE — the state you
have to capture to *resume* a guest instead of re-boot it), in <1000
lines:
https://github.com/ip888/Rust-nano-vm/blob/main/docs/blog/02-snapshot-restore.md

**Why I think this matters**: Firecracker is great but doesn't have a
native fork primitive. E2B is closed source at 150–400 ms cold start.
Containers share a kernel. For agent-eval workloads (fan out 1000
variants of one base snapshot, run for a few seconds, throw away) the
right shape is "fast fan-out + strong isolation", and nobody ships
that as an open building block.

**Stack**:
- Rust 1.94+, `kvm-ioctls`, `vm-memory`, `linux-loader`
- Custom virtio-vsock (~1200 lines, no `virtio-vsock` crate dep)
- Custom Prometheus exposition (no `prometheus` crate dep)
- `axum` REST control plane: bearer auth, per-token token-bucket quota
  on the expensive `/fork` route, per-caller usage metering,
  `/metrics` endpoint
- `MockHypervisor` for tests so CI doesn't need `/dev/kvm`

The whole workspace is ~12 crates, `cargo test --workspace` is green
without root, no unsafe outside the KVM ioctl shims.

**Honest caveats**:
- Pre-1.0. virtio-fs is a scaffold.
- Snapshots are same-host / same-kernel (no live migration; no
  cross-host portability).
- Control plane has auth + quota + metrics but no TLS — put nginx in
  front.
- I tested on a laptop. Real-world numbers on bare metal should be
  better, but the laptop numbers are honest.

**Reproduce** (Linux + KVM):

    git clone https://github.com/ip888/Rust-nano-vm.git
    cd Rust-nano-vm
    tools/kernel/build.sh
    tools/initramfs/build-initramfs.sh
    cargo run -p bench --features kvm --release -- --count 100 --alive 50

**Mock-backend demo** (no KVM):

    cargo build --release -p control-plane
    NANOVM_API_TOKENS=dev-token ./target/release/nanovm-control-plane &
    # then drive /v1/vms, /snapshot, /fork, /usage, /metrics — full demo
    # in the README.

Apache-2.0 / MIT. PRs and issues welcome. Particularly interested in:

- Production deployment war stories (what breaks at 100k forks/day?)
- containerd-shim integration (planned next; happy to take input on
  shape)
- People who want to use this for their own agent eval pipeline
  (testimony / case studies before 1.0 would be gold)

Solo project; if you're working on this problem (or hiring people who
work on it), I'd love to hear from you.

**Links**:
- Repo: https://github.com/ip888/Rust-nano-vm
- README: https://github.com/ip888/Rust-nano-vm#readme
- Architecture: https://github.com/ip888/Rust-nano-vm/blob/main/docs/architecture.md
- Comparison vs E2B/Firecracker/containers: https://github.com/ip888/Rust-nano-vm/blob/main/docs/comparison.md
```

## Flair

If /r/rust prompts you for flair, pick `project` (or `release` if it's
available). Not `help` or `meta`.

## After posting

- Reply to every top-level comment within ~2 hours of posting.
- Cross-post **once** to the HN thread by editing the HN first-comment
  to add `(also discussed on /r/rust: <link>)` — but don't do the
  reverse, /r/rust dislikes "from HN" framing.
- If someone asks "why not just use Firecracker", quote the answer
  from `README.md` in the playbook — don't reinvent it on the fly.
