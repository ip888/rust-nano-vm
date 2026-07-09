"""Agent-framework adapters — turn a nanovm sandbox into a first-class
tool an AI agent can call.

Two adapters ship today; both wrap the SAME
``POST /v1/sandbox/invoke`` endpoint the MCP bridge and the plain
``Client`` use, so a customer gets identical semantics regardless of
which frontend their agent runs through:

- :mod:`nanovm.agents.langchain` — a :class:`NanoVMTool` that plugs
  into LangChain / LangGraph agents via ``langchain-core``'s
  ``BaseTool`` interface. Install with ``pip install "nanovm[langchain]"``.

- :mod:`nanovm.agents.openai` — a plain JSON function-tool schema plus
  a dispatcher, for the OpenAI Assistants / Responses APIs (which
  speak function-tools natively). No third-party dependency; the
  standard ``nanovm`` install is enough.

Both adapters take a synchronous :class:`nanovm.Client` under the
hood — LangChain's ``_arun`` shim and OpenAI's dispatcher call the
sync ``.execute_python`` / ``.execute_shell`` methods. A future PR
will add first-class :class:`nanovm.AsyncClient` support for
frameworks driving true async I/O (LangGraph on a busy event loop,
FastAPI background workers). Pass an optional ``snapshot`` id to
target a **pre-built sandbox snapshot** (e.g. one that already has
``pandas``, ``numpy``, ``scikit-learn`` restored) so every tool call
is a warm-pool fork rather than a cold KVM boot.

Novel edge vs E2B / Modal Sandbox: because rust-nano-vm's fork is
~12 ms cold-start, an agent that hits its tool 100× per task pays
~1.2 s of sandbox overhead total — orders of magnitude below the
LLM response latency itself.
"""

from __future__ import annotations
