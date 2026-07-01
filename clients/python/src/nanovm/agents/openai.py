"""OpenAI Assistants / Responses API adapter.

OpenAI's tool-use APIs (Assistants, Responses, Chat Completions with
tools) take a JSON schema describing each tool and, when the model
decides to call one, hand you back the tool name + arguments to
dispatch yourself. This module gives you both halves for the nanovm
sandbox: a schema (:func:`tool_schemas`) and a dispatcher
(:func:`dispatch_tool_call`).

Basic Chat Completions flow::

    from openai import OpenAI
    import nanovm
    from nanovm.agents.openai import tool_schemas, dispatch_tool_call

    llm     = OpenAI()
    sandbox = nanovm.Client("http://localhost:8080", token="dev-token")
    tools   = tool_schemas()   # [{"type":"function","function":{...}}, ...]

    messages = [{"role": "user", "content": "Compute pi to 40 digits"}]
    while True:
        rsp = llm.chat.completions.create(
            model="gpt-4o", messages=messages, tools=tools,
        )
        msg = rsp.choices[0].message
        messages.append(msg)
        if not msg.tool_calls:
            print(msg.content)
            break
        for call in msg.tool_calls:
            result = dispatch_tool_call(sandbox, call.function.name, call.function.arguments)
            messages.append({
                "role": "tool", "tool_call_id": call.id, "content": result,
            })

No third-party OpenAI package is required to import this module — the
schema is plain dicts and the dispatcher takes strings. Bring your
own ``openai`` install (or ``anthropic``, or any client that consumes
OpenAI-shaped function-tools).
"""

from __future__ import annotations

import json
from typing import Any, Dict, List, Optional

from .. import Client, SandboxResult

__all__ = ["tool_schemas", "dispatch_tool_call"]


def tool_schemas() -> List[Dict[str, Any]]:
    """Return the OpenAI ``tools=[…]`` list for the two nanovm sandbox
    actions. Callable-shape (function-tool) — the same schema the
    Responses API and Chat Completions API both accept.

    Return format is a list of dicts with the shape::

        {
          "type": "function",
          "function": {
            "name": "execute_python",
            "description": "…",
            "parameters": { "$ref": "https://json-schema.org/…" },
          }
        }
    """
    return [
        {
            "type": "function",
            "function": {
                "name": "execute_python",
                "description": (
                    "Run a complete Python 3 program inside a fresh microVM "
                    "sandbox. Returns exit_code, stdout, and stderr. Use "
                    "whenever the task benefits from actually executing "
                    "code — computation, hypothesis testing, generating "
                    "output that depends on runtime values."
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "code": {
                            "type": "string",
                            "description": (
                                "A complete Python 3 program. stdout is "
                                "captured and returned. Each call is fully "
                                "isolated — no shared state between calls."
                            ),
                        }
                    },
                    "required": ["code"],
                    "additionalProperties": False,
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "execute_shell",
                "description": (
                    "Run a shell command (`sh -c <command>`) inside a fresh "
                    "microVM sandbox. Returns exit_code, stdout, and stderr. "
                    "Prefer this when you need a system binary (curl, git, "
                    "grep, apt, …)."
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to run.",
                        }
                    },
                    "required": ["command"],
                    "additionalProperties": False,
                },
            },
        },
    ]


def dispatch_tool_call(
    client: Client,
    name: str,
    arguments: str,
    *,
    snapshot: Optional[int] = None,
    timeout_ms: Optional[int] = None,
) -> str:
    """Dispatch an OpenAI tool call to the nanovm control plane.

    ``arguments`` is the JSON-encoded argument string OpenAI hands you
    on ``ToolCall.function.arguments`` — this function parses it and
    calls the right sandbox action. Returns a string suitable for the
    tool-role message body (``{"role":"tool", "content": <this>}``).

    Any exception from the client (network, auth, quota) is caught
    and returned as the tool result — the LLM sees the error and can
    self-correct rather than the whole agent loop crashing.
    """
    try:
        args = json.loads(arguments) if arguments else {}
    except json.JSONDecodeError as e:
        return f"error: could not parse tool arguments as JSON: {e}"

    try:
        if name == "execute_python":
            code = str(args.get("code", ""))
            result = client.execute_python(
                code=code, snapshot=snapshot, timeout_ms=timeout_ms
            )
        elif name == "execute_shell":
            command = str(args.get("command", ""))
            result = client.execute_shell(
                command=command, snapshot=snapshot, timeout_ms=timeout_ms
            )
        else:
            return f"error: unknown tool name {name!r}. Expected 'execute_python' or 'execute_shell'."
    except Exception as e:  # noqa: BLE001 — deliberate broad catch
        return f"error: {type(e).__name__}: {e}"

    return _format_result(result)


def _format_result(result: SandboxResult) -> str:
    """Compact but complete rendering — the model sees exit_code plus
    both output streams. Matches the LangChain adapter's shape so a
    prompt authored against one runs against the other.
    """
    parts = [f"exit_code={result.exit_code}"]
    if result.stdout:
        parts.append(f"stdout:\n{result.stdout.rstrip()}")
    if result.stderr:
        parts.append(f"stderr:\n{result.stderr.rstrip()}")
    return "\n".join(parts)
