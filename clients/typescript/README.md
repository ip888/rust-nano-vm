# @nanovm/sdk — TypeScript client for rust-nano-vm

TypeScript / JavaScript client for the
[rust-nano-vm](https://github.com/ip888/rust-nano-vm) REST control
plane. Zero runtime dependencies, ESM-only, works in Node 18+ and
every evergreen browser.

Mirrors the [Python SDK](../python/README.md) 1:1 in surface. If
you've used one you can read the other.

## Install

```sh
npm install @nanovm/sdk
```

Peer dep: `Node >= 18` (needs the built-in `fetch`).

## Give your AI agent a sandbox in three lines

```ts
import { Client } from "@nanovm/sdk";

const client = new Client("https://api.nanovm.example.com", {
  token: "nv_your-key",
});

console.log(
  (await client.executePython("print(sum(range(100)))", {
    snapshot: "python-3.12-minimal",
  })).stdout,
);
// "4950\n"
```

Or reuse one sandbox across many calls — fork once, pay `~12 ms` of
overhead per **session**, not per **call**:

```ts
await using sb = client.sandbox("python-3.12-ds");
await sb.executePython("import pandas as pd");                    // ~12 ms fork
await sb.executePython("df = pd.DataFrame({'x': [1, 2, 3]})");    // same VM
console.log((await sb.executePython("print(df.sum().to_dict())")).stdout);
```

`await using` is Node 20+ / TypeScript 5.2+. On older runtimes call
`sb.open()` and `sb.close()` explicitly (both idempotent, close swallows destroy
errors so a `finally { await sb.close() }` never masks the original error).

## Streaming exec

For long-running guest programs where you want output as it arrives —
log tailing, an agent loop, a build with progress — use `execStream`.
It's an async iterator over `ExecChunk` (stdout/stderr `Uint8Array`)
and a terminal `ExecExit`:

```ts
const vm = await client.forkMarketplace("python-3.12-minimal");
try {
  for await (const event of vm.execStream("python3", {
    args: ["-c", "for i in range(5): print(i, flush=True)"],
  })) {
    if (event.kind === "exit") {
      console.log("done", event.exit_code, event.duration_ms);
    } else {
      process.stdout.write(new TextDecoder().decode(event.data));
    }
  }
} finally {
  await vm.destroy();
}
```

The wire format is Server-Sent Events; the SDK parses + base64-decodes
chunks for you. Chunk boundaries follow the underlying transport — do
NOT assume one chunk per line. Errors raised BEFORE the stream opens
(`NotFoundError`, `ConflictError`, `AuthError`) surface synchronously;
errors mid-stream surface as `NanovmError` raised from the iterator.

## Errors

Every failure raises a typed exception derived from `NanovmError`:

```ts
import { NanovmError, NotFoundError, RateLimitedError } from "@nanovm/sdk";

try {
  await client.getVm(99999);
} catch (err) {
  if (err instanceof NotFoundError) {
    console.log(`VM doesn't exist: ${err.code} / ${err.message}`);
  }
}

try {
  await client.forkSnapshot(1);
} catch (err) {
  if (err instanceof RateLimitedError) {
    console.log(`hit fork quota; retry in ${err.retryAfter}s`);
  }
}
```

The `code` attribute is the server's stable machine-readable token
(e.g. `"unknown_vm"`, `"invalid_transition"`, `"too_many_requests"`).
Match on `code` rather than `message`; the message is free to change
between releases.

Full exception hierarchy:

| Class                  | HTTP  | Notes                                  |
|------------------------|-------|----------------------------------------|
| `NanovmError`          | any   | Base class. Carries `code` + `status`. |
| `AuthError`            | 401   | Bad or missing bearer token.           |
| `PaymentRequiredError` | 402   | Dunning-blocked. `upgradeEndpoint` points at `/v1/billing/portal`. |
| `NotFoundError`        | 404   | Unknown VM / snapshot id.              |
| `ConflictError`        | 409   | Invalid state transition.              |
| `RateLimitedError`     | 429   | `retryAfter` is seconds from `Retry-After`. |

5xx responses fold into a plain `NanovmError`, with `X-Request-Id`
included in `message` when the server surfaced one — makes support
tickets a copy-paste.

## Cursor pagination

```ts
// One page at a time.
const page = await client.listVms({ limit: 100 });
console.log(page.items, page.nextCursor);

// Or walk transparently.
let cursor: number | null = null;
do {
  const p = await client.listVms({ cursor: cursor ?? undefined, limit: 100 });
  for (const vm of p.items) console.log(vm.id, vm.state);
  cursor = p.nextCursor;
} while (cursor !== null);
```

## Health and usage

```ts
console.log(await client.health());
// { ok: true, backend: "mock", version: "0.0.3", uptime_secs: 42, started_at: "..." }

console.log(await client.usage());
// { token: "tok-dev--9", fork_count: 42, fork_total_ms: 520 }
```

## Framework adapters

**Not shipped in v0.1.** The `Client` + `Sandbox` API is designed to
drop straight into agent-framework tool definitions without an
adapter layer:

- **LangChain.js** — wrap `sb.executePython` in `new DynamicTool({...})`.
- **Vercel AI SDK** — pass `sb.executePython` inside `tool({ execute: ... })`.
- **Mastra** — construct a `createTool({...})` with `execute: sb.executePython`.

Purpose-built adapters (`@nanovm/sdk/langchain`, `@nanovm/sdk/vercel-ai`)
ship in a follow-up once there's demand.

## What this SDK is and isn't

**Is:**
- ESM-only, `Node >= 18`, uses global `fetch`.
- Zero runtime dependencies.
- 1:1 mirror of the REST surface documented in
  [`docs/openapi.json`](../../docs/openapi.json).
- Same DX as the Python SDK — `Client`, `Sandbox`, typed exceptions,
  `await using` for scope-bound sandbox lifecycle.

**Isn't:**
- A retry layer. Network errors raise `NanovmError`; wire retries at
  your call site (e.g. `p-retry`).
- An SSR-hostile browser bundle. It ships as one JS file with no
  side effects; a bundler can tree-shake unused DTOs freely.

## Versioning

Pre-1.0, expect churn aligned with the server. The SDK's version
tracks the server's `major.minor.patch`.

## License

Apache-2.0 OR MIT (same as the rust-nano-vm workspace).
