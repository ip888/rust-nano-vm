"""Tests for the two agent-framework adapters
(:mod:`nanovm.agents.langchain`, :mod:`nanovm.agents.openai`).

Both adapters wrap the sync ``nanovm.Client``; the tests use a stub
client so no server is required. The intent is to prove the adapter
shape (schemas + dispatch + result formatting), not to re-test the
underlying HTTP surface.
"""

from __future__ import annotations

import json
from types import SimpleNamespace

import pytest

import nanovm
from nanovm import SandboxResult


class _StubClient:
    """Minimal stand-in for :class:`nanovm.Client`. Records the last
    call and returns a scripted :class:`SandboxResult`.
    """

    def __init__(self, result: SandboxResult):
        self._result = result
        self.calls = []

    def execute_python(self, code, snapshot=None, timeout_ms=None):
        self.calls.append(("execute_python", {"code": code, "snapshot": snapshot, "timeout_ms": timeout_ms}))
        return self._result

    def execute_shell(self, command, snapshot=None, timeout_ms=None):
        self.calls.append(("execute_shell", {"command": command, "snapshot": snapshot, "timeout_ms": timeout_ms}))
        return self._result


def _ok(stdout="42\n", stderr="", exit_code=0):
    return SandboxResult(
        stdout=stdout, stderr=stderr, exit_code=exit_code, duration_ms=15, cold_start=False
    )


# ---- LangChain --------------------------------------------------------


def test_langchain_tool_bundle_shape():
    from nanovm.agents.langchain import NanoVMTool, NanoVMPythonTool, NanoVMShellTool

    client = _StubClient(_ok())
    tools = NanoVMTool(client, snapshot=7)
    assert isinstance(tools, list)
    assert len(tools) == 2
    kinds = {t.name for t in tools}
    assert kinds == {"execute_python", "execute_shell"}
    assert isinstance(tools[0], NanoVMPythonTool)
    assert isinstance(tools[1], NanoVMShellTool)


def test_langchain_python_tool_dispatches_and_formats():
    from nanovm.agents.langchain import NanoVMPythonTool

    client = _StubClient(_ok(stdout="42\n"))
    tool = NanoVMPythonTool(client=client, snapshot=7, timeout_ms=2000)

    # LangChain BaseTool.invoke is the public entrypoint since
    # langchain-core 0.3. Falls back to _run in older versions.
    if hasattr(tool, "invoke"):
        out = tool.invoke({"code": "print(6*7)"})
    else:  # pragma: no cover
        out = tool._run(code="print(6*7)")

    assert "exit_code=0" in out
    assert "42" in out
    call_name, args = client.calls[0]
    assert call_name == "execute_python"
    assert args["code"] == "print(6*7)"
    assert args["snapshot"] == 7
    assert args["timeout_ms"] == 2000


def test_langchain_shell_tool_dispatches():
    from nanovm.agents.langchain import NanoVMShellTool

    client = _StubClient(_ok(stdout="hi\n"))
    tool = NanoVMShellTool(client=client, snapshot=7)
    out = tool.invoke({"command": "echo hi"}) if hasattr(tool, "invoke") else tool._run(command="echo hi")
    assert "hi" in out
    assert client.calls == [("execute_shell", {"command": "echo hi", "snapshot": 7, "timeout_ms": None})]


def test_langchain_tool_surfaces_stderr_in_output():
    from nanovm.agents.langchain import NanoVMPythonTool

    client = _StubClient(_ok(stdout="", stderr="Traceback (most recent call last)…", exit_code=1))
    tool = NanoVMPythonTool(client=client)
    out = tool.invoke({"code": "raise Exception()"}) if hasattr(tool, "invoke") else tool._run(code="raise Exception()")
    assert "exit_code=1" in out
    assert "stderr" in out and "Traceback" in out


# ---- OpenAI --------------------------------------------------------


def test_openai_schema_shape_matches_function_tool_spec():
    from nanovm.agents.openai import tool_schemas

    schemas = tool_schemas()
    assert isinstance(schemas, list) and len(schemas) == 2
    for s in schemas:
        assert s["type"] == "function"
        fn = s["function"]
        assert "name" in fn and "description" in fn and "parameters" in fn
        params = fn["parameters"]
        assert params["type"] == "object"
        assert "properties" in params
        assert isinstance(params.get("required", []), list)

    names = {s["function"]["name"] for s in schemas}
    assert names == {"execute_python", "execute_shell"}


def test_openai_dispatch_python_success():
    from nanovm.agents.openai import dispatch_tool_call

    client = _StubClient(_ok(stdout="42\n"))
    args_json = json.dumps({"code": "print(6*7)"})
    out = dispatch_tool_call(client, "execute_python", args_json, snapshot=3)

    assert "exit_code=0" in out
    assert "42" in out
    assert client.calls == [
        ("execute_python", {"code": "print(6*7)", "snapshot": 3, "timeout_ms": None})
    ]


def test_openai_dispatch_shell_success():
    from nanovm.agents.openai import dispatch_tool_call

    client = _StubClient(_ok(stdout="drwx"))
    out = dispatch_tool_call(client, "execute_shell", json.dumps({"command": "ls /"}))
    assert "drwx" in out
    assert client.calls[0][0] == "execute_shell"


def test_openai_dispatch_unknown_tool_returns_error_string():
    from nanovm.agents.openai import dispatch_tool_call

    client = _StubClient(_ok())
    out = dispatch_tool_call(client, "delete_the_universe", "{}")
    assert out.startswith("error:")
    assert "delete_the_universe" in out
    assert client.calls == []  # never dispatched


def test_openai_dispatch_bad_json_returns_error_string():
    from nanovm.agents.openai import dispatch_tool_call

    client = _StubClient(_ok())
    out = dispatch_tool_call(client, "execute_python", "not valid json")
    assert out.startswith("error:")
    assert client.calls == []


def test_openai_dispatch_client_error_surfaces_to_llm_not_raise():
    """Simulate a client-side exception during dispatch. The adapter
    must return it as a string so the agent loop sees the error and
    can self-correct, rather than crashing the whole conversation.
    """
    from nanovm.agents.openai import dispatch_tool_call

    class ExplodingClient:
        def execute_python(self, **_):
            raise nanovm.RateLimited("throttled", code="quota_exceeded", status=429, retry_after=1)

    out = dispatch_tool_call(ExplodingClient(), "execute_python", json.dumps({"code": "pass"}))
    assert out.startswith("error:")
    assert "RateLimited" in out or "throttled" in out
