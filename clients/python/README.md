# nanovm — Python client for rust-nano-vm

A thin synchronous wrapper around the
[rust-nano-vm](https://github.com/ip888/Rust-nano-vm) REST control
plane. Designed to read like the lifecycle a user actually drives,
not like a generated stub of the OpenAPI.

## Install

```sh
pip install nanovm                        # sync client only (requests)
pip install "nanovm[async]"               # + AsyncClient (httpx)
pip install "nanovm[langchain]"           # + LangChain NanoVMTool
pip install "nanovm[openai]"              # + OpenAI Assistants adapter
pip install "nanovm[agents]"              # everything above
```

Or, from a working checkout (development install):

```sh
pip install ./clients/python
```

## `nanovm` command-line client

`pip install nanovm` also puts a `nanovm` command on your PATH.
Shares its config file with the SDK — one identity per host.

```sh
nanovm login --api-url https://api.your-saas.com
# API key (input hidden): ***                    # or --key on CLI / NANOVM_API_KEY env

nanovm status
#  Org           acme
#  API           https://api.your-saas.com
#  Forks         42
#  Total ms      523
#  Avg ms/fork   12

nanovm python 'print(sum(range(100)))'
#  4950
nanovm shell 'uname -a'
#  Linux …

nanovm logout                                    # forget the saved key
```

Config lives at `$XDG_CONFIG_HOME/nanovm/config.json` on Linux and
`~/.nanovm/config.json` on macOS/Windows; override with the
`NANOVM_CONFIG` env var. File perms are 0600 on Unix.

## Give your AI agent a real sandbox in three lines

LangChain / LangGraph:

```python
import nanovm
from nanovm.agents.langchain import NanoVMTool
from langchain_openai import ChatOpenAI
from langgraph.prebuilt import create_react_agent

sandbox = nanovm.Client("https://api.nanovm.example.com", token="acme-…")
tools   = NanoVMTool(sandbox, snapshot=12)   # 12 = a pre-built python-data-science snapshot

agent = create_react_agent(ChatOpenAI(model="gpt-4o"), tools)
agent.invoke({"messages": [("user", "Compute pi to 40 digits")]})
```

Each tool call is a fresh microVM fork — `~12 ms` on real KVM. An agent that hits its tool 100× per task pays `~1.2 s` of sandbox overhead total. Compare to E2B (`150-400 ms × 100 = 30-40 s`) or Modal Sandbox (`~200 ms × 100 = 20 s`).

OpenAI Assistants / Responses / Chat Completions:

```python
from openai import OpenAI
import nanovm
from nanovm.agents.openai import tool_schemas, dispatch_tool_call

llm     = OpenAI()
sandbox = nanovm.Client("http://localhost:8080", token="dev-token")
tools   = tool_schemas()          # plain dicts, ready to pass to `tools=`

messages = [{"role": "user", "content": "Compute pi to 40 digits"}]
while True:
    rsp = llm.chat.completions.create(model="gpt-4o", messages=messages, tools=tools)
    msg = rsp.choices[0].message
    messages.append(msg)
    if not msg.tool_calls:
        print(msg.content); break
    for call in msg.tool_calls:
        result = dispatch_tool_call(sandbox, call.function.name, call.function.arguments)
        messages.append({"role": "tool", "tool_call_id": call.id, "content": result})
```

## Async client

```python
import asyncio, nanovm

async def main():
    async with nanovm.AsyncClient("http://localhost:8080", token="dev-token") as client:
        # Retries 429/502/503/504 with exponential backoff; honours Retry-After.
        result = await client.execute_python("print(1 + 1)")
        print(result.stdout)          # "2\n"

asyncio.run(main())
```

PyPI releases are cut by [`.github/workflows/python-publish.yml`](../../.github/workflows/python-publish.yml)
on every `v*.*.*` tag push, using PyPI Trusted Publishers (no API
token stored in the repo). Until a tag has shipped through the
workflow, the path install is the only one that works.

## 30-second smoke test

Start a control plane somewhere — Docker is easiest:

```sh
docker run -d --rm -p 8080:8080 -e NANOVM_API_TOKENS=dev-token \
    ghcr.io/ip888/nanovm-control-plane:latest
```

Then in Python:

```python
import nanovm

with nanovm.Client("http://localhost:8080", token="dev-token") as client:
    vm = client.create_vm()
    vm.start()

    result = vm.exec(
        program="python3",
        args=["-c", "print(1+1)"],
    )
    print(result.stdout)      # "2\n"
    print(result.exit_code)   # 0

    vm.destroy()
```

(The exec returns `"2\n"` only when the control plane is wired to a
real KVM backend with a Python-equipped guest rootfs — the published
Docker image runs the mock backend, so `exit_code` will surface the
mock's response. The API call shape is identical either way.)

## Snapshot + fork

The headline primitive — a customer eval loop fanning out N variants
of a warm base image:

```python
import nanovm

with nanovm.Client("http://localhost:8080", token="dev-token") as client:
    base = client.create_vm()
    base.start()
    # (warm the base however your workload needs)
    snap = base.snapshot()

    # 1000 forks, ~12 ms each on real KVM
    children = []
    for _ in range(1000):
        children.append(snap.fork())

    # Run something different in each
    for vm in children:
        result = vm.exec(program="python3", args=["-c", "import os; print(os.getpid())"])
        print(result.stdout.strip())
        vm.destroy()
```

## Sandbox-action API (one-shot fork-exec-destroy)

For AI-agent tool use, you usually don't want to manage VM lifecycles
— you just want "run this in a sandbox and give me back the output."
`Client.execute_python`, `Client.execute_shell`, `Client.read_file`,
`Client.write_file`, and `Client.list_files` each do that as a single
call: the server forks a fresh VM from a snapshot, runs the action,
destroys the VM, and returns a flat `SandboxResult`.

```python
import nanovm

with nanovm.Client("http://localhost:8080", token="dev-token") as client:
    # The server's NANOVM_SANDBOX_SNAPSHOT_ID env var picks the
    # default; pass `snapshot=` to override per-call.
    r = client.execute_python("print(1+1)", snapshot=42)
    print(r.stdout)        # "2\n"
    print(r.cold_start)    # True (cold-restored) or False (warm pool)

    r = client.execute_shell("uname -a", snapshot=42)
    r = client.read_file("/etc/hostname", snapshot=42)
    r = client.write_file("/tmp/x", "hello", mode=0o644, snapshot=42)
    r = client.list_files("/tmp", snapshot=42)
```

`SandboxResult.exit_code` follows POSIX-shell convention
(signal-killed processes are reported as `128 + signal`). For
direct access to the underlying endpoint with an arbitrary action
name, use `Client.sandbox_invoke(action, snapshot=..., **kwargs)`.

## Streaming exec

For long-running guest programs where you want output as it arrives —
log tailing, an LLM agent loop, a build with progress — use
`Vm.exec_stream` instead of `Vm.exec`. It's a generator over
`ExecChunk` (stdout/stderr bytes) and a terminal `ExecExit`:

```python
import nanovm

with nanovm.Client("http://localhost:8080", token="dev-token") as client:
    vm = client.create_vm()
    vm.start()

    for event in vm.exec_stream(
        program="python3",
        args=["-c", "for i in range(5): print(i, flush=True)"],
    ):
        if isinstance(event, nanovm.ExecChunk):
            print(event.kind, event.data)            # b"0\n", b"1\n", …
        else:                                         # ExecExit, terminal
            print("done", event.exit_code, event.duration_ms)

    vm.destroy()
```

The wire format is Server-Sent Events; the SDK parses + base64-decodes
chunks for you. Chunk boundaries follow the underlying transport — do
not assume one chunk per line. Errors raised before the stream opens
(`NotFoundError`, `ConflictError`, `AuthError`) surface synchronously;
errors mid-stream surface as `NanovmError` raised from the iterator.

## Errors

Every failure raises a typed exception derived from `NanovmError`:

```python
import nanovm

try:
    vm = client.get_vm(99999)
except nanovm.NotFoundError as e:
    print(f"VM doesn't exist: {e.code} / {e}")

try:
    snap.fork()
except nanovm.RateLimited as e:
    print(f"hit fork quota; retry in {e.retry_after}s")
```

The `code` attribute is the server's stable machine-readable token
(e.g. `"unknown_vm"`, `"invalid_transition"`, `"too_many_requests"`).
Match on `code` rather than `str(e)`; the human-readable message is
free to change between releases.

## Cursor pagination

```python
# One page at a time
page = client.list_vms(limit=100)            # newest 100

# Or walk the whole result set transparently
for vm in client.iter_vms(page_size=100):
    print(vm.id, vm.state)
```

## Health and usage

```python
h = client.health()
# Health(ok=True, backend='mock', version='0.0.3', uptime_secs=42, started_at='...')

u = client.usage()
# Usage(token='tok-dev--9', fork_count=42, fork_total_ms=520)
```

## What this SDK is and isn't

**Is**:

- A synchronous client, ~400 lines, one `requests` dependency.
- A 1:1 mirror of the REST surface documented in
  [`docs/openapi.json`](../../docs/openapi.json), with Pythonic
  ergonomics (dataclasses, typed exceptions, context-manager close).
- Stable enough for the eval-pipeline use case in
  [blog post #4](../../docs/blog/04-12ms-eval-fanout.md).

**Isn't**:

- An async client. If you need `asyncio`, wrap calls in
  `asyncio.to_thread` for now; an `httpx`-based async variant lands
  if there's demand.
- A retry layer. Network errors raise `NanovmError`; pin your retry
  policy at your call site (tenacity is the idiomatic choice).

## Versioning

Pre-1.0, expect churn aligned with the server. The SDK's `__version__`
moves with the server's major.minor.patch.

## License

Apache-2.0 OR MIT (same as the rust-nano-vm workspace).
