# Run your local LLM's agent tools inside real microVMs

> **Three terminals. Your GPU stays busy on the model, your laptop stays
> safe from AI-written code.** No cloud spend, no API keys leaving your
> machine, ~12 ms per tool call.

This walkthrough shows how to pair a locally-hosted LLM (Ollama /
llama.cpp / vLLM / LM Studio) with a `rust-nano-vm` sandbox so that
every tool call your AI agent makes — `execute_python`,
`execute_shell`, `read_file`, `write_file` — runs in a real
`/dev/kvm` microVM instead of your host process. Compared to letting
the model shell out directly, or wrapping it in a Docker container,
you get:

|                        | Direct exec | Docker    | **`rust-nano-vm` local** | E2B / Modal (cloud) |
| ---------------------- | ----------- | --------- | ------------------------ | ------------------- |
| Isolation              | none        | weak      | **real microVM**         | real microVM        |
| Kernel separation      | ✗           | shared    | **✓ (guest kernel)**     | ✓                   |
| Cold-start             | 0 ms        | ~200 ms   | **~12 ms**               | 150–400 ms + network |
| Cost                   | free        | free      | **free**                 | $0.02–0.05 / task   |
| Data leaves your box   | no          | no        | **no**                   | yes                 |
| Setup effort           | trivial     | small     | **small (this doc)**     | account + API key   |

Nothing sensitive ever leaves your laptop: the model runs local, the
sandbox runs local, the agent code is yours. This is the sweet spot for
developers who use their laptop for LLM experimentation *because* it
saves money vs cloud APIs — you'd want to keep it that way.

## Wait — should you use `langchain-sandbox` (Pyodide) instead?

[LangChain shipped](https://www.langchain.com/blog/running-untrusted-agent-code-without-a-sandbox)
`langchain-sandbox`, which runs Python via Pyodide (CPython → WASM)
inside Deno's permission-flag runtime. Zero infrastructure — install a
package, run in-process, ~50-100 ms cold-start.

**Honestly: if your model only writes pure Python against stdlib +
Pyodide-compatible wheels (numpy, pandas, scipy), and it never needs
`pip install X`, or a shell command, or a native binary — `langchain-sandbox`
is faster and simpler. Use it.**

Come back to this guide the moment any of these become true:

- The model needs `pip install torch` / `playwright` / `opencv` / anything native
- The model writes `subprocess.run(["curl", …])` / `git` / `apt install`
- The agent's tool needs to leave state on the filesystem between calls
- You need a real per-user audit log or per-tenant hardware isolation
- The tool call needs to run a long-lived process (Jupyter kernel, dev server)

Full comparison across 13 axes: [`docs/comparison.md`](comparison.md#vs-langchain-sandbox-pyodide--deno).

## Prerequisites

- **Linux with `/dev/kvm`** — Omarchy, Arch, Ubuntu, Fedora, any distro
  where `ls -l /dev/kvm` shows a `crw-rw----` character device owned by
  the `kvm` group. Verify with `egrep -c '(vmx|svm)' /proc/cpuinfo`
  (should be `> 0`). See [`docs/kvm-host.md`](kvm-host.md) if the
  device is missing.
- **Docker + docker compose** — for the sandbox stack. Podman-compose
  works too.
- **Python 3.10+** — for the agent code.
- **Any local model runner.** This guide uses [Ollama](https://ollama.com)
  as the example, but the shape is identical for llama.cpp,
  vLLM, LM Studio, oobabooga's text-generation-webui, or anything
  exposing an OpenAI-compatible endpoint on `localhost`.

## Terminal 1 — start your local LLM

```sh
# Ollama example — swap for llama.cpp / vLLM / LM Studio as you prefer.
ollama serve &
ollama pull llama3.1:8b            # or codellama, qwen2.5-coder, deepseek-coder-v2 …
```

Ollama exposes an OpenAI-compatible endpoint at
`http://localhost:11434/v1`, so any LangChain / OpenAI SDK code that
speaks the OpenAI Chat Completions API works verbatim — you just point
`base_url` at Ollama instead of `api.openai.com`.

If you're using llama.cpp: `./llama-server --host 0.0.0.0 --port 8000
-m your-model.gguf --api-key sk-local`. If vLLM:
`vllm serve meta-llama/Llama-3.1-8B-Instruct --port 8000`. In both
cases the endpoint speaks OpenAI-compatible JSON.

## Terminal 2 — start the nanovm sandbox

```sh
git clone https://github.com/ip888/Rust-nano-vm && cd Rust-nano-vm
cd deploy/live-demo
./up-local.sh
```

`up-local.sh` (from [PR #136](https://github.com/ip888/Rust-nano-vm/pull/136))
preflights `/dev/kvm`, builds the KVM image locally if it's not
already published, mints per-org bearer tokens into `.env.local`, and
starts three docker containers:

| Container | What it does |
| --- | --- |
| `nanovm-demo-control-plane` | Real `nanovm-control-plane-kvm` binary. Opens `/dev/kvm` from your host. |
| `nanovm-demo-prometheus`    | Scrapes `/metrics` at 15 s intervals. |
| `nanovm-demo-grafana`       | Dashboard at `localhost:3000/d/nanovm-overview`. |

When it prints `✓ Local live-KVM demo is running.`, verify:

```sh
curl -fsS http://localhost:8080/v1/health \
  -H "Authorization: Bearer $(source .env.local; echo $ACME_TOKEN)" | jq
# → {"ok":true,"backend":"kvm-fleet",…}
```

Grab the `ACME_TOKEN` for the next step — that's the API key your
agent code will use.

## Terminal 3 — your agent

Install the SDK's LangChain extra. `uv` and `pip` both work — pick
your preference:

```sh
# uv (fast Rust-based resolver)
uv add "nanovm[langchain]" langchain-openai langgraph

# pip
pip install "nanovm[langchain]" langchain-openai langgraph
```

Then write your agent — point the LLM at Ollama, point the tools at
your local nanovm:

```python
# agent.py
import os
from langchain_openai import ChatOpenAI
from langgraph.prebuilt import create_react_agent
import nanovm
from nanovm.agents.langchain import NanoVMTool

# LLM: local Ollama (no API cost, no data leaves the box).
llm = ChatOpenAI(
    model="llama3.1:8b",
    base_url="http://localhost:11434/v1",
    api_key="ollama",              # Ollama doesn't check this; any string works.
    temperature=0.2,
)

# Tools: local nanovm sandbox. Every execute_* call is a real KVM fork.
sandbox = nanovm.Client(
    "http://localhost:8080",
    token=os.environ["ACME_TOKEN"],
)
tools = NanoVMTool(sandbox)        # [NanoVMPythonTool, NanoVMShellTool]

agent = create_react_agent(llm, tools)

result = agent.invoke({
    "messages": [
        ("user", "Compute pi to 40 decimal places using Python, then verify "
                 "the 27th digit is a 3."),
    ],
})
for m in result["messages"]:
    print(f"[{m.type}] {m.content}")
```

Run it:

```sh
source deploy/live-demo/.env.local     # exports ACME_TOKEN + friends
python agent.py
```

While it runs, watch **`localhost:3000/d/nanovm-overview`**. The
*Forks / sec (by org)* panel spikes each time the model decides to
call `execute_python`; the *Fork latency p50 / p99* panel shows the
real KVM restore times (`~12 ms` p50 on a stock i5 laptop after the
first warm-pool refill).

## OpenAI-Assistants shape (no LangChain)

If you'd rather not pull LangChain in, the OpenAI adapter is
zero-dep — schemas are plain dicts:

```python
from openai import OpenAI
import nanovm
from nanovm.agents.openai import tool_schemas, dispatch_tool_call

llm      = OpenAI(base_url="http://localhost:11434/v1", api_key="ollama")
sandbox  = nanovm.Client("http://localhost:8080", token=os.environ["ACME_TOKEN"])
tools    = tool_schemas()          # → [{"type":"function",...}, ...]

messages = [{"role": "user", "content": "Compute pi to 40 digits and print it."}]
while True:
    rsp = llm.chat.completions.create(
        model="llama3.1:8b", messages=messages, tools=tools,
    )
    msg = rsp.choices[0].message
    messages.append(msg)
    if not msg.tool_calls:
        print(msg.content); break
    for call in msg.tool_calls:
        result = dispatch_tool_call(sandbox, call.function.name, call.function.arguments)
        messages.append({"role": "tool", "tool_call_id": call.id, "content": result})
```

Same everything — LLM local, sandbox local, tool calls sandboxed in real KVM.

## Claude Desktop / Cursor via MCP

If your "agent" is Claude Desktop or Cursor rather than a Python script,
the [`nanovm-mcp`](../crates/nanovm-mcp) binary exposes the same
sandbox actions as MCP tools your editor's LLM can call:

```jsonc
// ~/.config/Claude/claude_desktop_config.json (or Cursor's equivalent)
{
  "mcpServers": {
    "nanovm": {
      "command": "/path/to/nanovm-mcp",
      "env": {
        "NANOVM_BASE_URL": "http://localhost:8080",
        "NANOVM_TOKEN":    "your-token-from-.env.local"
      }
    }
  }
}
```

Claude's or Cursor's built-in AI now sees `execute_python`,
`execute_shell`, `read_file`, `write_file`, `list_files` as tools — and
every call runs in real KVM on your laptop.

## Choosing a pre-built sandbox snapshot

By default, each `NanoVMTool` call cold-starts a fresh microVM. That's
fine for basic `print()` / `subprocess` work, but if your model
routinely needs pandas / numpy / scikit-learn / requests, cold-installing
those on every call is wasted work.

The fix: **create the snapshot once, fork forever**.

```sh
# One-time: create + start a VM, install what you want available,
# snapshot it. `snapshot_id` is the number your agent tools reference.
export TOKEN=$(source deploy/live-demo/.env.local; echo $ACME_TOKEN)
vm=$(curl -fsX POST http://localhost:8080/v1/vms \
     -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
     -d '{}' | jq .id)
curl -fsX POST http://localhost:8080/v1/vms/$vm/start -H "Authorization: Bearer $TOKEN"
curl -fsX POST http://localhost:8080/v1/vms/$vm/exec \
     -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
     -d '{"cmd":["/bin/sh","-c","pip install pandas numpy scikit-learn"]}'
snapshot=$(curl -fsX POST http://localhost:8080/v1/vms/$vm/snapshot \
           -H "Authorization: Bearer $TOKEN" | jq .id)
echo "snapshot id: $snapshot"     # → e.g. 42
```

Then in your agent:

```python
tools = NanoVMTool(sandbox, snapshot=42)   # every call forks the DS-ready snapshot
```

Set `NANOVM_WARM_POOL_PER_SNAPSHOT=8` in `deploy/live-demo/compose/docker-compose.local.yml`'s
`control-plane` service env if you want a hot pool of pre-restored
children — turns `12 ms` p50 into `~1-2 ms` on cache-hot calls.

## Cost / power / latency notes

- **Idle:** the `control-plane` container is a Rust binary with a
  couple of megabytes of RSS. Prometheus and Grafana add another few
  hundred megabytes RAM but only scrape once per 15 s so CPU sits at
  0 % between scrapes. No power drain unless a fork happens.
- **Per fork:** ~12 ms wall-clock + ~0.5 MiB Pss (proportional set size)
  per child. A model that hits its tool 100× per task pays under 1.5 s
  of sandbox overhead and ~50 MiB total memory, all reclaimed at
  `.destroy()`.
- **Comparison:** running the same 100-tool-call task against E2B or
  Modal Sandbox pays ~20–40 s of network + cold-start on top of the
  actual model latency, and $0.02–$0.05 per task depending on tier.
  Both add up when you're iterating.

## Troubleshooting

- **`/v1/health` reports `"backend":"mock"`.** The KVM image isn't
  running — you're on the default mock binary. Rebuild with the KVM
  target (`up-local.sh` handles this; re-run and watch its output for
  the pull/build fallback).
- **`docker exec nanovm-demo-control-plane ls -l /dev/kvm`** doesn't
  show the device. Your host's `kvm` group GID is different from what
  the container expects. `up-local.sh` auto-detects it via
  `stat -c '%g' /dev/kvm`; if you ran the compose file by hand, export
  `KVM_GID=$(stat -c '%g' /dev/kvm)` before `docker compose up`.
- **The model calls tools but the results say `exit_code=127`.**
  Whatever the model asked for isn't installed in the default
  sandbox. Either broaden the model's system prompt to stick to
  stdlib, or bake a snapshot (see "Choosing a pre-built sandbox
  snapshot" above) with the tools it needs.
- **Grafana loads but every panel says "No data".** Prometheus hasn't
  had time to complete its first scrape (~15 s from `up-local.sh`
  finishing). Wait a bit; refresh.

## Related

- [`deploy/live-demo/README.md`](../deploy/live-demo/README.md) — full
  walkthrough of the two live-demo paths (local KVM + Fly.io).
- [`clients/python/README.md`](../clients/python/README.md) — SDK
  reference including the agent adapters.
- [`crates/nanovm-mcp/`](../crates/nanovm-mcp) — MCP bridge for
  Claude Desktop / Cursor / Claude Code integration.
- [`docs/blog/04-12ms-eval-fanout.md`](blog/04-12ms-eval-fanout.md) —
  the reason `~12 ms` cold-start matters for agent-loop UX.
