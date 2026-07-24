# Give Claude Code a real sandbox in three lines

> **TL;DR.** Claude Code (and every other agent CLI that runs
> `Bash` / `execute_python`) needs a real sandbox for anything past
> a demo. Docker works but takes 100–500 ms per call and shares a
> kernel with your laptop. A KVM microVM with snapshot+fork gives
> you the same "just run it" ergonomics at ~12 ms per call, with a
> hardware boundary between the agent's untrusted tool calls and
> your machine. Three lines to wire it up.

## The problem — a coding agent needs a shell

Claude Code, Cursor Agent, Devin, OpenHands, aider — every serious
coding agent bottoms out in "execute this shell command / this
Python snippet". Without a sandbox, that means the model can:

- delete files under your `$HOME`
- exfiltrate secrets from your `~/.aws/credentials` or your shell
  history
- `pip install` a package that runs a postinstall script that owns
  the machine
- get stuck in an infinite loop that pins a CPU core forever

The common fixes are:

- **Trust the model** — fine for a demo, terrifying in production
  or on your own laptop for anything past yesterday's homework.
- **Docker exec** — real defence in depth, but ~100–500 ms of
  cold-start per call (see [blog post
  #4](04-12ms-eval-fanout.md) for the breakdown), and containers
  share a kernel with the host so a kernel-level escape (rare,
  historic, real) crosses your trust boundary.
- **A VM** — the right primitive on paper, but nobody wants to
  wait 30 seconds per tool call for QEMU to boot.

**nanovm** takes the "microVM with snapshot + fork" primitive and
wraps it as a one-line SDK call. `~12 ms` cold-start against a
warm pool. Real KVM boundary. No shared kernel.

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

Give Claude Code a tool that shells out into a fresh microVM per
call:

```python
def execute_shell(cmd: str) -> str:
    r = client.execute_shell(cmd, snapshot="python-3.12-minimal")
    return f"exit={r.exit_code}\n{r.stdout}\n{r.stderr}"
```

Wire that into Claude Code's MCP surface (or into your Claude Code
tool config, or into the Anthropic Messages API's `tools=[...]` if
you're driving the model directly) and every `Bash` action the
agent takes runs in a ~12 ms fork of a real Linux VM. The agent
sees exactly the same output shape as before; the failure mode of
"model deletes $HOME" is now "model deletes a VM that dies in
milliseconds anyway".

## What "sandboxed" actually means here

- **KVM microVM.** Each call is a hardware-virtualised guest with
  its own kernel, its own rootfs, its own memory. Not a
  namespace inside your host — a real `/dev/kvm` boot.
- **~12 ms fork against the warm pool.** The control plane keeps
  N pre-restored children per snapshot in a background queue; the
  fork endpoint pops one and hands you a running VM. Cold-restore
  (empty pool) is <30 ms.
- **Destroyed after the call.** By default `execute_shell` /
  `execute_python` is fork → run → destroy. No state carries
  across calls. If you WANT state (a Jupyter kernel between
  calls, a shared filesystem), open a `Sandbox` and reuse the
  VM:

```python
with client.sandbox(snapshot="python-3.12-ds") as sb:
    sb.execute_python("import pandas as pd")                  # ~12 ms fork
    sb.execute_python("df = pd.DataFrame({'x': [1, 2, 3]})")  # same VM
    print(sb.execute_python("print(df.sum().to_dict())").stdout)
```

## Sizing the win vs Docker exec

The relevant number isn't "raw fork latency"; it's **how long the
agent waits per tool call, integrated over a task**. Claude Code
sessions typically emit dozens of `Bash` calls per task; a real
agent-eval loop might emit hundreds.

| Sandbox layer   | Per-call cold-start | 50-call agent task |
| --------------- | ------------------- | ------------------ |
| Docker exec     | 50–200 ms           | 2.5–10 s           |
| E2B             | 150–400 ms          | 7.5–20 s           |
| Modal Sandbox   | ~200 ms             | ~10 s              |
| **nanovm**      | **~12 ms**          | **~0.6 s**         |

The overhead-per-task delta is what makes nanovm feel like "there
is no sandbox" and lets you leave it turned on for every agent
run — evals, dev-loop, prod — instead of only when you remember.

## Three follow-up things you can do

1. **Fork-once-reuse-N** — the `Sandbox` context manager amortises
   the ~12 ms across every call in a `with` block. If your agent
   session is coherent enough to reuse one VM, per-call latency
   drops to sub-ms after the first fork.
2. **Pre-built snapshots** — the [marketplace](../architecture.md)
   ships ready-to-fork Python / Node / shell images so you don't
   build one yourself. `snapshot="python-3.12-ds"` picks up a
   pandas + numpy + scipy image.
3. **Per-org fork quota** — the control plane enforces
   `NANOVM_FORK_RPS` per token, so a runaway agent can't exhaust
   your budget in a loop. `RateLimited` surfaces as a typed
   exception so your retry policy is one `except` clause.

## Try it

There's a [free tier](https://nanovm.example.com/pricing) with
5 forks/sec + 10K forks/month for hobby use, or you can
self-host — the whole thing is Apache 2.0 / MIT dual-licensed.

Full setup + code lives at
[github.com/ip888/rust-nano-vm](https://github.com/ip888/rust-nano-vm).
