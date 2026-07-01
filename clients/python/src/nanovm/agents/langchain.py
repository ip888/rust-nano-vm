"""LangChain adapter — turn a nanovm sandbox into a LangChain tool.

The typical shape::

    from langchain_openai import ChatOpenAI
    from langgraph.prebuilt import create_react_agent
    import nanovm
    from nanovm.agents.langchain import NanoVMTool

    client = nanovm.Client("https://api.nanovm.example.com", token="acme-…")
    tool   = NanoVMTool(client, snapshot=12)   # pre-built "python-3.12-ds" snapshot

    agent = create_react_agent(ChatOpenAI(model="gpt-4o"), [tool])
    agent.invoke({"messages": [("user", "Compute the correlation matrix …")]})

The tool exposes two functions to the LLM: ``execute_python`` and
``execute_shell``. Both are dispatched to
``POST /v1/sandbox/invoke`` and return the sandbox's captured
stdout / stderr / exit-code.

Install: ``pip install "nanovm[langchain]"``.
"""

from __future__ import annotations

from typing import Any, Optional, Type

try:
    from langchain_core.tools import BaseTool
    from pydantic import BaseModel, Field
except ImportError as e:  # pragma: no cover
    raise ImportError(
        "nanovm.agents.langchain requires 'langchain-core' + 'pydantic'. "
        "Install with: pip install 'nanovm[langchain]'"
    ) from e

from .. import Client, SandboxResult

__all__ = ["NanoVMTool", "NanoVMPythonTool", "NanoVMShellTool"]


# ---- Input schemas (Pydantic v2 shape) -----------------------------


class _PythonInput(BaseModel):
    """Argument schema for the ``execute_python`` tool."""

    code: str = Field(
        description=(
            "A complete Python 3 program to run inside an isolated microVM. "
            "stdout is captured and returned; stderr is captured separately. "
            "No shared state across calls — each invocation forks a fresh sandbox."
        ),
    )


class _ShellInput(BaseModel):
    """Argument schema for the ``execute_shell`` tool."""

    command: str = Field(
        description=(
            "A shell command (`sh -c <command>`) to run inside an isolated microVM. "
            "stdout / stderr / exit-code are captured and returned."
        ),
    )


# ---- Tools ----------------------------------------------------------


class _NanoVMToolBase(BaseTool):
    """Shared base for the two per-action tools. Not intended for
    direct use — callers instantiate :class:`NanoVMPythonTool` or
    :class:`NanoVMShellTool`, or use the aggregating
    :class:`NanoVMTool` helper below.
    """

    # Typed as `Any` deliberately: real callers pass a
    # :class:`nanovm.Client`, but the adapter only needs the two
    # duck-typed methods (`execute_python`, `execute_shell`).
    # Pydantic v2's strict typing would otherwise reject a subclass or
    # a legitimate test stub. Callers get their type checking from the
    # sync client itself.
    client: Any
    snapshot: Optional[int] = None
    timeout_ms: Optional[int] = None

    model_config = {"arbitrary_types_allowed": True}

    def _fmt(self, result: SandboxResult) -> str:
        # Return a compact but complete string so the agent's next
        # thought sees exit code + both streams. LangChain expects a
        # string (or list of strings) back from a tool call.
        parts = [f"exit_code={result.exit_code}"]
        if result.stdout:
            parts.append(f"stdout:\n{result.stdout.rstrip()}")
        if result.stderr:
            parts.append(f"stderr:\n{result.stderr.rstrip()}")
        return "\n".join(parts)


class NanoVMPythonTool(_NanoVMToolBase):
    """LangChain tool that runs Python code in an isolated nanovm sandbox.

    LangChain reads ``name``, ``description``, and ``args_schema``
    off the class to build the LLM-facing tool spec. The description
    is what steers the model toward "use me when you need to run
    Python code" — keep it explicit.
    """

    name: str = "execute_python"
    description: str = (
        "Run a complete Python 3 program inside a fresh microVM sandbox. "
        "Returns exit_code, stdout, stderr. Use this whenever you need "
        "to compute something, verify a hypothesis, or run a snippet of "
        "code. Each call is fully isolated — no shared state between calls."
    )
    args_schema: Type[BaseModel] = _PythonInput

    def _run(self, code: str, **_: Any) -> str:  # type: ignore[override]
        result = self.client.execute_python(
            code=code,
            snapshot=self.snapshot,
            timeout_ms=self.timeout_ms,
        )
        return self._fmt(result)

    async def _arun(self, code: str, **_: Any) -> str:  # type: ignore[override]
        # Sync client under the hood — LangChain's async execution is
        # a background thread by default. A future PR can plumb
        # AsyncClient here for real async under a tokio-backed loop.
        return self._run(code)


class NanoVMShellTool(_NanoVMToolBase):
    """LangChain tool that runs a shell command in an isolated nanovm sandbox."""

    name: str = "execute_shell"
    description: str = (
        "Run a shell command (`sh -c <command>`) inside a fresh microVM "
        "sandbox. Returns exit_code, stdout, stderr. Prefer this when you "
        "need to invoke a system binary (grep, curl, git, apt, …)."
    )
    args_schema: Type[BaseModel] = _ShellInput

    def _run(self, command: str, **_: Any) -> str:  # type: ignore[override]
        result = self.client.execute_shell(
            command=command,
            snapshot=self.snapshot,
            timeout_ms=self.timeout_ms,
        )
        return self._fmt(result)

    async def _arun(self, command: str, **_: Any) -> str:  # type: ignore[override]
        return self._run(command)


def NanoVMTool(  # noqa: N802 — callable factory, not a class
    client: Client,
    snapshot: Optional[int] = None,
    timeout_ms: Optional[int] = None,
) -> list["_NanoVMToolBase"]:
    """Return ``[NanoVMPythonTool, NanoVMShellTool]`` — the standard
    "give my agent a sandbox" bundle. Pass the list directly to
    ``create_react_agent`` / ``AgentExecutor``::

        agent = create_react_agent(llm, NanoVMTool(client, snapshot=12))

    For a subset — say "just Python, no shell" — construct one of the
    concrete tool classes directly.
    """
    return [
        NanoVMPythonTool(client=client, snapshot=snapshot, timeout_ms=timeout_ms),
        NanoVMShellTool(client=client, snapshot=snapshot, timeout_ms=timeout_ms),
    ]
