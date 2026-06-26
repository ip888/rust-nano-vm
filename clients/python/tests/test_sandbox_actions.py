"""Offline unit tests for the sandbox-action SDK methods.

Unlike ``test_smoke.py`` (which needs a live control plane and
auto-skips otherwise), these tests stub the underlying
``requests.Session`` so they always run — even in CI without a
server. The point is to lock in:

- the request body shape sent to ``POST /v1/sandbox/invoke``
  (action discriminator + per-action fields + snapshot precedence)
- the ``SandboxResult`` parse from the canonical server envelope
- the surface of typed exceptions on 4xx/5xx
"""

from __future__ import annotations

from typing import Any, Dict, List, Tuple
from unittest.mock import MagicMock

import pytest

import nanovm


class FakeResponse:
    """Minimal stand-in for `requests.Response` returning canned JSON."""

    def __init__(
        self,
        status_code: int = 200,
        json_body: Optional[Dict[str, Any]] = None,
    ):
        self.status_code = status_code
        self._json = json_body if json_body is not None else {}
        # `_request` reads `.content` to decide whether to parse JSON,
        # and `.text` is the fallback in `_raise_for_error`. Use a
        # non-empty bytes blob so the JSON path is taken.
        import json as _json
        self.content = _json.dumps(self._json).encode("utf-8")
        self.text = self.content.decode("utf-8")
        self.headers: Dict[str, str] = {}

    def json(self) -> Any:
        return self._json


@pytest.fixture
def client_with_stub() -> Tuple[nanovm.Client, List[Dict[str, Any]]]:
    """Client whose `requests.Session.request` is patched to capture
    every outgoing call and reply with a canned SandboxResult envelope.

    Returns ``(client, captured)`` — `captured` is a list of every
    request kwargs dict, in order, so a test can assert on body
    shape.
    """
    c = nanovm.Client("http://stub", token="t")
    captured: List[Dict[str, Any]] = []

    def fake_request(method: str, url: str, **kwargs: Any) -> FakeResponse:
        captured.append({"method": method, "url": url, **kwargs})
        return FakeResponse(
            status_code=200,
            json_body={
                "stdout": "hello\n",
                "stderr": "",
                "exit_code": 0,
                "duration_ms": 17,
                "cold_start": True,
            },
        )

    c._session.request = MagicMock(side_effect=fake_request)  # type: ignore[method-assign]
    return c, captured


def _last_body(captured: List[Dict[str, Any]]) -> Dict[str, Any]:
    """Convenience: last captured request's JSON body."""
    return captured[-1]["json"]


# --- envelope parsing --------------------------------------------------------


def test_sandbox_result_parses_canonical_envelope(client_with_stub) -> None:
    c, _ = client_with_stub
    result = c.execute_shell("echo hello", snapshot=1)
    assert isinstance(result, nanovm.SandboxResult)
    assert result.stdout == "hello\n"
    assert result.stderr == ""
    assert result.exit_code == 0
    assert result.duration_ms == 17
    assert result.cold_start is True


# --- body shape per action ---------------------------------------------------


def test_execute_python_body_shape(client_with_stub) -> None:
    c, captured = client_with_stub
    c.execute_python("print(1)", snapshot=7, timeout_ms=500)
    body = _last_body(captured)
    assert body == {
        "action": "execute_python",
        "snapshot": 7,
        "code": "print(1)",
        "timeout_ms": 500,
    }


def test_execute_python_omits_optional_fields(client_with_stub) -> None:
    c, captured = client_with_stub
    c.execute_python("pass", snapshot=1)
    body = _last_body(captured)
    assert "timeout_ms" not in body
    assert body["action"] == "execute_python"
    assert body["code"] == "pass"


def test_execute_shell_body_shape(client_with_stub) -> None:
    c, captured = client_with_stub
    c.execute_shell("ls /", snapshot=1, timeout_ms=2000)
    body = _last_body(captured)
    assert body == {
        "action": "execute_shell",
        "snapshot": 1,
        "command": "ls /",
        "timeout_ms": 2000,
    }


def test_read_file_body_shape(client_with_stub) -> None:
    c, captured = client_with_stub
    c.read_file("/etc/hostname", snapshot=1)
    body = _last_body(captured)
    assert body == {
        "action": "read_file",
        "snapshot": 1,
        "path": "/etc/hostname",
    }


def test_write_file_body_shape_with_mode(client_with_stub) -> None:
    c, captured = client_with_stub
    c.write_file("/tmp/x", "hello", mode=0o755, snapshot=1)
    body = _last_body(captured)
    assert body == {
        "action": "write_file",
        "snapshot": 1,
        "path": "/tmp/x",
        "content": "hello",
        "mode": 0o755,
    }


def test_write_file_omits_mode_when_unset(client_with_stub) -> None:
    c, captured = client_with_stub
    c.write_file("/tmp/x", "data", snapshot=1)
    body = _last_body(captured)
    assert "mode" not in body
    assert body["content"] == "data"


def test_list_files_body_shape(client_with_stub) -> None:
    c, captured = client_with_stub
    c.list_files("/tmp", snapshot=1)
    body = _last_body(captured)
    assert body == {
        "action": "list_files",
        "snapshot": 1,
        "path": "/tmp",
    }


# --- snapshot precedence -----------------------------------------------------


def test_snapshot_omitted_when_caller_doesnt_pass_it(client_with_stub) -> None:
    """No `snapshot` field means the server falls back to its env var."""
    c, captured = client_with_stub
    c.execute_shell("true")
    body = _last_body(captured)
    assert "snapshot" not in body


def test_sandbox_invoke_low_level_dispatch(client_with_stub) -> None:
    """The escape-hatch method accepts any action + free-form kwargs."""
    c, captured = client_with_stub
    c.sandbox_invoke("execute_python", snapshot=42, code="print(2)")
    body = _last_body(captured)
    assert body == {"action": "execute_python", "snapshot": 42, "code": "print(2)"}


# --- error mapping -----------------------------------------------------------


def test_sandbox_invoke_404_raises_not_found_error() -> None:
    """Unknown-snapshot response surfaces as a typed NotFoundError."""
    c = nanovm.Client("http://stub", token="t")
    err_envelope = {"error": {"code": "unknown_snapshot", "message": "nope"}}

    def fake_request(method: str, url: str, **kwargs: Any) -> FakeResponse:
        return FakeResponse(status_code=404, json_body=err_envelope)

    c._session.request = MagicMock(side_effect=fake_request)  # type: ignore[method-assign]
    with pytest.raises(nanovm.NotFoundError) as exc:
        c.execute_shell("true", snapshot=999_999)
    assert exc.value.code == "unknown_snapshot"
    assert exc.value.status == 404


def test_sandbox_invoke_400_raises_generic_nanovm_error() -> None:
    """Missing snapshot id surfaces with the server's `bad_request` code."""
    c = nanovm.Client("http://stub", token="t")
    err_envelope = {
        "error": {
            "code": "bad_request",
            "message": "no snapshot id: pass `snapshot` in body or set NANOVM_SANDBOX_SNAPSHOT_ID",
        }
    }

    def fake_request(method: str, url: str, **kwargs: Any) -> FakeResponse:
        return FakeResponse(status_code=400, json_body=err_envelope)

    c._session.request = MagicMock(side_effect=fake_request)  # type: ignore[method-assign]
    with pytest.raises(nanovm.NanovmError) as exc:
        c.execute_shell("true")
    assert exc.value.code == "bad_request"
    assert exc.value.status == 400
