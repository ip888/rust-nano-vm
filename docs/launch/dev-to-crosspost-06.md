---
title: "Give your LangChain.js agent execute_python in three lines"
published: false
description: Zero-dep TypeScript client + OpenAI-shape tool descriptors. Drops into LangChain.js, Vercel AI SDK, and Anthropic tool use without adapter code.
tags: ai, agents, typescript, javascript
canonical_url: https://nanovm.example.com/blog/06-langchain-js-execute-python
cover_image: https://nanovm.example.com/opengraph-image
series: nanovm — sandboxes for AI agents
---

> This is a mirror of the same post at
> [nanovm.example.com/blog/06](https://nanovm.example.com/blog/06-langchain-js-execute-python) —
> the canonical URL is set in the frontmatter above so search
> engines credit the original.

## The Node.js agent story

If you're building an agent in Node, your sandbox options are:

- **Docker exec via `dockerode`** — real isolation, but every tool call is a ~100–500 ms Docker round-trip. A LangGraph ReAct loop that hits `execute_python` 50 times pays 5–25 s of pure sandbox overhead.
- **`vm2` / `isolated-vm`** — runs the model's JS inside your Node process. No isolation from the host if the model wanders outside the sandbox (which it will). Also Python-blind.
- **Roll your own remote sandbox** — write a REST service that spins up a container per call. This is what most people do and it's a lot of glue.
- **Just trust the model** — the industry's actual default. No.

**nanovm** slots into slot #3 without the glue:

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

That's a real Chat Completions call, a real `execute_python` tool
schema wired via `bindTools`, a real ~12 ms fork of a real KVM
microVM, and the tool output threaded back for the next ReAct turn.

## Why `bindTools` "just works"

The OpenAI function-tool JSON-Schema shape is the lingua franca:

- **LangChain.js** — `bindTools` accepts it directly.
- **Vercel AI SDK** — `streamText({tools: ...})` accepts it with a two-line `jsonSchema()` wrap.
- **OpenAI Assistants / Responses / Chat Completions** — verbatim.
- **Anthropic tool use** — `parameters` → `input_schema`, otherwise identical.

`@nanovm/sdk/agents` returns exactly that shape from `nanovmToolSchemas()`, and `dispatchNanovmToolCall()` handles the inverse: parses the model's `arguments` JSON, invokes the right sandbox action, catches every possible failure into an `error:` string the model can self-correct against on its next turn. Zero peer deps, zero adapter version drift.

## Fork once, run many

For agent sessions where a single task emits multiple tool calls, forking one VM per call is wasteful. Use `Sandbox`:

```ts
await using sb = sandbox.sandbox("python-3.12-ds");
await sb.executePython("import pandas as pd");                   // ~12 ms fork
await sb.executePython("df = pd.DataFrame({'x': [1, 2, 3]})");   // same VM
console.log((await sb.executePython("print(df.sum().to_dict())")).stdout);
```

`await using` fires the destructor on scope exit (Node ≥ 20 / TS ≥ 5.2). On older runtimes, `sb.open()` and `sb.close()` are explicit.

## Try it

Free tier: 5 forks/sec + 10K/month. No card.

👉 **[nanovm.example.com](https://nanovm.example.com)** — signup + in-browser playground
👉 **[/pricing](https://nanovm.example.com/pricing)** — Free / Pro $29/mo / Team $199/mo
👉 **[github.com/ip888/rust-nano-vm](https://github.com/ip888/rust-nano-vm)** — Apache 2.0 / MIT
