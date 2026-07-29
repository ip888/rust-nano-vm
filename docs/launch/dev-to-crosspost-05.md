---
title: Give Claude Code a real sandbox in three lines
published: false
description: Every coding-agent CLI needs a shell that won't nuke your $HOME. Here's a ~12 ms KVM microVM you can drop in.
tags: ai, agents, python, kvm
canonical_url: https://nanovm.example.com/blog/05-sandbox-for-claude-code
cover_image: https://nanovm.example.com/opengraph-image
series: nanovm — sandboxes for AI agents
---

> This is a mirror of the same post at
> [nanovm.example.com/blog/05](https://nanovm.example.com/blog/05-sandbox-for-claude-code) — the
> canonical URL is set in the frontmatter above so search
> engines credit the original.

## The problem — a coding agent needs a shell

Claude Code, Cursor Agent, Devin, OpenHands, aider — every
serious coding agent bottoms out in "execute this shell command /
this Python snippet". Without a sandbox, that means the model can:

- delete files under your `$HOME`
- exfiltrate secrets from your `~/.aws/credentials`
- `pip install` a package that runs a postinstall script that
  owns the machine
- pin a CPU core in an infinite loop

The common fixes are:

- **Trust the model** — fine for a demo, terrifying past yesterday's homework.
- **Docker exec** — real defence, but 100-500 ms cold-start per call, and the container shares a kernel with the host.
- **A VM** — right on paper, but nobody wants to wait 30 seconds per tool call for QEMU to boot.

**nanovm** takes the "microVM with snapshot + fork" primitive and
wraps it as a one-line SDK call. `~12 ms` cold-start against a warm
pool. Real KVM boundary. No shared kernel.

## Three lines

Install:

```sh
pip install nanovm
```

Point the SDK at your control plane (self-hosted or the hosted
service — same API):

```python
import nanovm

client = nanovm.Client("https://api.nanovm.example.com", token="nv_...")
```

Wire it into your Claude Code tool config so every `Bash` action
runs in a fresh microVM:

```python
def execute_shell(cmd: str) -> str:
    with client.sandbox(snapshot="python-3.12-minimal") as sb:
        r = sb.execute_shell(cmd)
        return f"exit={r.exit_code}\n{r.stdout}\n{r.stderr}"
```

The `sandbox()` context manager accepts a marketplace snapshot
name directly. If you'd rather manage the snapshot id yourself:

```python
snap = client.fork_marketplace("python-3.12-minimal").snapshot()

def execute_shell(cmd: str) -> str:
    r = client.execute_shell(cmd, snapshot=snap.id)
    return f"exit={r.exit_code}\n{r.stdout}\n{r.stderr}"
```

## Sizing the win vs Docker exec

The relevant number isn't "raw fork latency"; it's **how long the
agent waits per tool call, integrated over a task**.

| Sandbox layer   | Per-call cold-start | 50-call agent task |
| --------------- | ------------------- | ------------------ |
| Docker exec     | 50–200 ms           | 2.5–10 s           |
| E2B             | 150–400 ms          | 7.5–20 s           |
| Modal Sandbox   | ~200 ms             | ~10 s              |
| **nanovm**      | **~12 ms**          | **~0.6 s**         |

## Try it

Free tier: 5 forks/sec + 10K forks/month. No credit card.

There's an in-browser playground behind signup — paste Python,
hit Run, watch it execute in a real KVM microVM in under a
second. That's the "oh, this actually works" moment.

👉 **[nanovm.example.com](https://nanovm.example.com)** —
signup + playground
👉 **[/pricing](https://nanovm.example.com/pricing)** — Free / Pro
$29/mo / Team $199/mo / Enterprise
👉 **[github.com/ip888/rust-nano-vm](https://github.com/ip888/rust-nano-vm)** —
Apache 2.0 / MIT dual-licensed
