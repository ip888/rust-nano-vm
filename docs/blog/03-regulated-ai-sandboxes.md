# Running AI agents in regulated environments: what microVMs buy you over containers

> **TL;DR.** If your AI agent runs LLM-generated code and you're in
> healthcare, finance, defense, or any EU-residency-bound context, your
> SaaS sandbox options are gone before you start. Containers don't
> close the gap on their own. A microVM does — and a single-binary one
> means a sandbox you can actually attest to from your audit boundary.

## The setup

An AI agent generates code. It needs to run that code somewhere — to
return a result, to call a tool, to reproduce a bug in a customer's
data. Maybe the code is from a model you trust; maybe it's from a
patient-uploaded plugin; maybe it's from a contractor whose laptop you
haven't audited. Either way, that code is **untrusted** by your
production environment's standards.

In an unregulated startup you reach for E2B, Modal, Daytona, or some
other managed code-execution SaaS. Five lines of SDK code, done.

In a regulated environment, every one of those is off the table:

- **HIPAA-covered workloads** can't ship PHI off-premise without a BAA
  most of those vendors don't sign at startup scale.
- **PCI-DSS** environments treat third-party code execution on PAN
  data as a scoping nightmare.
- **EU data-residency** rules (GDPR, the AI Act, sector-specific
  banking and health rules) require known-jurisdiction processing.
- **Defense** (FedRAMP, IL4/5/6, ITAR, NATO) won't let your packets
  leave the boundary, full stop.
- **Confidentiality-by-contract** (consultancies, law, IP-heavy
  enterprise) often can't legally allow a third party to see the
  inputs.

You build on-prem or you don't build.

## Why "just use a container" doesn't finish the job

Containers (Docker, containerd, runc, Podman) share a kernel with the
attacker's code. That's a security posture, not a security boundary,
and your security team knows it.

What containers actually give you:

1. **Linux namespaces** — process / mount / network isolation. Useful.
   Routinely escaped through kernel bugs (Dirty Pipe, OverlayFS races,
   io_uring patches, the regular Tuesday CVE).
2. **Linux capabilities** — drop what you don't need. Necessary.
   Doesn't help if your kernel has a privilege-escalation bug.
3. **Seccomp filters** — block syscalls you don't need. Necessary.
   Doesn't help against syscalls you *do* need that turn out to have
   bugs.
4. **AppArmor / SELinux** — mandatory access control. Necessary.
   Doesn't substitute for hardware-level isolation.

These are the right things to do. But:

- **HIPAA auditors don't recognize "containers share a kernel" as
  acceptable isolation between your code and patient data**, and they
  shouldn't. Same shape of conversation under PCI, SOC 2 Type II,
  ISO 27001, FedRAMP.
- **gVisor (runsc)** is genuinely better — it interposes a userspace
  kernel — but it costs you 15–50% on syscalls and 20–30% on memory
  bandwidth, and your auditor still asks the same question.
- **Kata Containers** is a microVM under a container interface — same
  technical model as the rest of this article, but with the
  container-tooling tax (image format, OCI runtimes, jailer).

You can build a secure container-based sandbox. You just have to
explain it the same way every security review for the next ten years.

## What a microVM actually changes

A microVM (Firecracker, Cloud Hypervisor, rust-nano-vm) gives you a
real virtualization boundary — not a namespace boundary. The guest
runs on its own Linux kernel, its own page tables, its own ring 0.
The host kernel can be a different version with a different patch
level. An attacker who roots the guest gets the guest. To get the
host, they need a KVM CVE.

Three things flow from that for a compliance-aware deployment:

1. **The boundary maps onto your audit narrative.** "Untrusted code
   executes in a hardware-isolated VM whose kernel is the only attack
   surface" is a sentence an auditor can sign off on. "Untrusted code
   executes in a container that shares the host kernel" is the
   sentence that starts the back-and-forth.

2. **Ephemerality is the default, not a feature you bolt on.** A
   microVM started from a snapshot, used for one request, and
   destroyed leaves no persistent state on the host. There's no
   `/var/lib/docker` layer cache holding executed code.

3. **The blast radius is measurable.** Memory caps, vCPU caps, vsock-
   only host connectivity, no shared filesystems. The list of things
   the guest *can* do is short and inspectable.

## Where rust-nano-vm fits

[`rust-nano-vm`](https://github.com/ip888/Rust-nano-vm) is a
single-binary host-side Rust microVM aimed at AI-agent code
execution. The Rust source is `#[forbid(unsafe_code)]` everywhere
except the `vm-kvm` crate, which concentrates the project's only
`unsafe` in two small areas: the
[`MAP_PRIVATE` CoW fork primitive](01-mmap-private.md) and the KVM
vCPU state save/restore. Five `unsafe` blocks across the entire
workspace.

For a regulated deployment, four properties matter more than any
single benchmark number:

### 1. One host binary, no external services

The host-side surface — control plane, VMM, snapshot/fork logic —
ships as one binary (`nanovm-control-plane`). There's no jailer
process, no orchestrator daemon, no managed cloud account, no Docker
daemon. The guest VM does run a small static-musl `nanovm-agent`
inside its own kernel (baked into an initramfs you build and pin),
but that runs in the guest, not on the host — its attack surface
faces *into* the sandbox, not out of it.

That makes the host binary itself attestable. You can produce its
SHA256, sign it with cosign/sigstore, pin its hash in your
inventory, and have your security team review the build provenance
one time.

The release workflow builds prebuilt
[`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, and `aarch64-apple-darwin`](https://github.com/ip888/Rust-nano-vm/releases/latest)
binaries on GitHub Actions with `--locked` against a checked-in
`Cargo.lock`. Each release carries a sidecar SHA256 you can verify
before deployment. (The real KVM backend is Linux-only; the macOS
build runs the mock backend for development on Apple Silicon.)

### 2. Bearer-token auth on every mutating route

Every `/v1/*` route requires a bearer token. Tokens are configured
via `NANOVM_API_TOKENS` (comma-separated, set at startup, never
written to logs). The control plane records a non-cryptographic
fingerprint (`tok-<first4>-<len>`) for usage metering so leaked
audit logs don't leak tokens.

A misconfiguration (empty token list) emits a loud `WARN` on
startup, not a silent fallback. Catches "we forgot to set the env
var in production" before it ships.

### 3. No outbound telemetry

There is no phone-home. There is no analytics endpoint. There is no
update check. The control plane only listens; it never initiates a
connection beyond what the guest itself does.

If your network team's firewall blocks everything outbound from the
sandbox host, `rust-nano-vm` works. You won't see "Service Unavailable"
errors from a service you didn't know existed.

### 4. Snapshot+fork is the unit of execution

The headline performance feature (~12 ms cold start via
`MAP_PRIVATE` CoW) is also a compliance feature. Every untrusted
execution is a fresh fork of a known-good snapshot. There is no
shared mutable state between executions. Forensic "what did session
X see" questions reduce to "what was in the snapshot" plus "what did
session X write" — both of which are bounded files on disk.

## What this is *not*

Crucial honesty. None of this is a substitute for a security review,
and none of this is what your auditor needs to issue an attestation:

- **Not a turnkey HIPAA solution.** `rust-nano-vm` is one technical
  control. HIPAA compliance covers policy, training, BAAs, breach
  response, ePHI minimization. The microVM helps with §164.312(a)(1)
  (Access Control) and §164.312(b) (Audit Controls). Everything else
  is still your job.
- **Not FedRAMP attested.** No FedRAMP authorization, no Moderate or
  High baseline. If you operate inside an authorized boundary, the
  microVM is a building block; the boundary work is yours.
- **Not SOC 2.** No SOC 2 Type II report. The pieces that map to SOC 2
  controls (logging, access control, change management) are present
  but they're *your* controls in *your* deployment.
- **Not a managed service.** Updates, patches, OS-level hardening on
  the host running the microVM are your responsibility. There is no
  vendor SLA because there is no vendor.

If those gaps eliminate it for your use case, that's the right call.
If you're already prepared to own the deployment, the microVM removes
one specific class of risk (kernel-shared isolation) that's expensive
to argue for under audit.

## Deployment pattern

The shape we're seeing make sense for regulated deployments:

```
                    ┌──────────────────────────────────┐
                    │  Your application                │
                    │  (your existing audit boundary)  │
                    └──────────────┬───────────────────┘
                                   │ HTTPS, bearer token
                                   ▼
                    ┌──────────────────────────────────┐
                    │  Reverse proxy + TLS termination │
                    │  (nginx / Caddy / your existing) │
                    └──────────────┬───────────────────┘
                                   │ plain HTTP on loopback
                                   ▼
                    ┌──────────────────────────────────┐
                    │  rust-nano-vm control plane      │
                    │  Bind 127.0.0.1:8080             │
                    │  NANOVM_API_TOKENS=<rotated>     │
                    │  /metrics → Prometheus           │
                    └──────────────┬───────────────────┘
                                   │ KVM ioctl + virtio
                                   ▼
                    ┌──────────────────────────────────┐
                    │  Forked guest VMs                │
                    │  - no host filesystem mount      │
                    │  - vsock-only egress             │
                    │  - bounded RAM + vCPU            │
                    └──────────────────────────────────┘
```

Concretely:

- **Bind to loopback only.** The control plane listens on
  `127.0.0.1:8080` by default. Don't expose it on `0.0.0.0` even
  behind a firewall — let your existing reverse proxy do TLS, auth,
  WAF, request body limits.
- **Rotate the bearer token monthly.** Tokens are revocable by
  restarting the binary with a new `NANOVM_API_TOKENS=…`. Roll one in,
  drain the old one out the next day.
- **Scrape `/metrics`.** Prometheus exposition is unauthenticated by
  design so scrapers don't carry secrets. Block it at the proxy so
  it's only reachable from your monitoring subnet.
- **Disable outbound on the guest's vsock.** The guest agent runs in
  the VM; if you don't need it to phone home (you don't), filter the
  vsock CIDs at the host level.

## Try it

Two evaluation paths, depending on whether you want to build from
source or audit a prebuilt binary.

**From source** (needs a Rust toolchain — `rustup` from
https://rustup.rs). No KVM required; the demo runs against the mock
backend:

```sh
git clone https://github.com/ip888/Rust-nano-vm
cd Rust-nano-vm
cargo run -p control-plane --example demo --release
```

For real numbers on a Linux + KVM host:

```sh
cargo run -p bench --features kvm --release --bin nanovm-fork-bench -- \
    --count 100 --alive 50 --settle-secs 2
```

**Prebuilt binary** (no Rust toolchain on the host). Released for
`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, and
`aarch64-apple-darwin` from the
[releases page](https://github.com/ip888/Rust-nano-vm/releases/latest),
each with a sidecar SHA256. Example for Linux x86_64:

```sh
curl -L https://github.com/ip888/Rust-nano-vm/releases/latest/download/rust-nano-vm-VERSION-x86_64-unknown-linux-gnu.tar.gz | tar xz
cd rust-nano-vm-VERSION-x86_64-unknown-linux-gnu
NANOVM_API_TOKENS=evaluation ./nanovm-control-plane
```

## What's interesting if you take this seriously

If `rust-nano-vm` is a candidate for your regulated AI-agent deployment
and you have ideas about the gaps above (audit log format, token
rotation primitives, FIPS-aligned crypto, vsock policy, hardware-rooted
attestation), file an issue. Pre-1.0 means there's room for the
contract to reflect what real auditors actually ask.

The repo lives at https://github.com/ip888/Rust-nano-vm.

---

Companion posts:

- [How rust-nano-vm cold-starts in ~12 ms: `mmap(MAP_PRIVATE)` is the whole trick](01-mmap-private.md)
- [Faithful KVM snapshot/restore in <1000 lines of Rust](02-snapshot-restore.md)
