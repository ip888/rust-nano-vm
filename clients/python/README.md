# nanovm — Python client for rust-nano-vm

A thin synchronous wrapper around the
[rust-nano-vm](https://github.com/ip888/Rust-nano-vm) REST control
plane. Designed to read like the lifecycle a user actually drives,
not like a generated stub of the OpenAPI.

## Install

```sh
pip install nanovm
```

Or, from a working checkout (development install):

```sh
pip install ./clients/python
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
