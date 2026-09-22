# CLAUDE.md — working rules for `rust-nano-vm`

## Product direction (as of 2026-09-21)

The project's product line is **sub-second cold-start and fork for
enterprise Java workloads** — primarily Spring Boot, secondarily Quarkus
and Micronaut. The core primitives (KVM snapshot/restore, MAP_PRIVATE
fork-many, jailer, control-plane) are DB-agnostic and JDK-vendor-agnostic
by design: we do not build or ship per-database snapshots (Postgres,
Oracle, MySQL, …) and do not integrate with any specific DB licensing or
masking stack. Customers bring their own images and their own database
wherever it lives.

The earlier AI-agent code-execution positioning (Python/TS SDKs, MCP
bridge, marketplace of curated snapshots, live Fly.io demo, Stripe
consumer billing) was cut in the Java-enterprise pivot cleanup. Its full
state is preserved on the `pre-java-pivot` branch for reference.

## PR lifecycle: review comments are part of "ready"

**Before reporting that a PR is "ready to merge" or marking it out of
draft, ALWAYS check Copilot / human review comments on that PR and
resolve every technically valid one in the same PR (or as an explicit
follow-up the user has agreed to).**

Lesson from session 2026-06-24: shipped PRs #98, #99, #102, #103 without
checking review comments, then discovered 19 legitimate Copilot findings
after merge — including real bugs (RPC race on shared `RequestId(1)`,
unbounded SSE channel, missing-Exit silent fall-through). Had to land a
follow-up sweep (#104) that should have been part of the original PRs.

**The check is cheap and the cost of skipping it is real.** After pushing
a branch and creating the PR, before flipping to ready-for-review:

1. `mcp__github__pull_request_read` with `method=get_review_comments` and
   the PR number.
2. For each unresolved thread:
   - **Tractable bug** → fix in-place, push, repeat.
   - **Tractable doc/test nit** → fix in-place, push, repeat.
   - **Ambiguous / architecturally significant** → ask the user via
     `AskUserQuestion` before deciding.
   - **Genuinely out-of-scope** → skip with a short explanation in the
     PR description.
3. Only then mark ready / report "ready to merge" to the user.

If Copilot hasn't posted yet (it usually takes ~5 min), schedule a
short `ScheduleWakeup` or just wait — `webhook` notifies on review
comments. Don't claim "no review comments yet" as final.

This rule applies to **every** PR opened from this repo, not just ones
the user explicitly asks to check.

## Other notes

- The workspace's `kvm` feature must stay opt-in. `cargo test --workspace`
  needs to be green without `/dev/kvm` so portable CI works.
- Real-KVM integration tests under `crates/vm-kvm/tests/*_boot.rs`
  follow a skip-when-fixtures-missing pattern. New ones should match it.
- Feature branches must use the `claude/<topic>` prefix per the session's
  branch-naming requirement.
- Target platforms (v1): Linux + KVM on x86_64 (Intel VT-x, AMD-V).
  OpenShift and vanilla Kubernetes are first-class deployment targets.
  ARM64 (aarch64) is a v2 goal; the KVM backend is x86-specific today.
- Do NOT reintroduce AI-agent-specific code paths (marketplace, MCP
  bridge, per-user Stripe signup, browser playground). Recover from
  `pre-java-pivot` only if a specific piece needs to be reused
  intentionally.
