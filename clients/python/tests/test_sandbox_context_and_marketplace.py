"""Offline unit tests for the reusable Sandbox context manager, the
`fork_marketplace` helper, and the `PaymentRequiredError` typed
exception.

Same stub pattern as `test_sandbox_actions.py` — patches
`requests.Session.request` so nothing hits a real server. The point
is to lock in:

- `Client.sandbox(snapshot=int)`  → forks `/v1/snapshots/:id/fork` on enter
- `Client.sandbox(snapshot=str)`  → forks the marketplace endpoint on enter
- Subsequent `sandbox.execute_python()` calls reuse the SAME VM id
  (proves fork-once semantics)
- Exit destroys the VM even on exception in the `with` block
- 402 responses raise `PaymentRequiredError` with `upgrade_endpoint`
- 5xx responses include the server's `X-Request-Id` in the message
"""

from __future__ import annotations

import json as _json
from typing import Any, Dict, List, Optional, Tuple
from unittest.mock import MagicMock

import pytest

import nanovm


class FakeResponse:
    def __init__(
        self,
        status_code: int = 200,
        json_body: Optional[Dict[str, Any]] = None,
        headers: Optional[Dict[str, str]] = None,
    ):
        self.status_code = status_code
        self._json = json_body if json_body is not None else {}
        self.content = _json.dumps(self._json).encode("utf-8")
        self.text = self.content.decode("utf-8")
        self.headers = headers or {}

    def json(self) -> Any:
        return self._json


def _scripted_client(
    handler: Any,
) -> Tuple[nanovm.Client, List[Dict[str, Any]]]:
    """Client whose `requests.Session.request` is driven by `handler`
    — a callable `(method, url, **kwargs) -> FakeResponse`. Returns
    the client + a captured list of outgoing calls."""
    c = nanovm.Client("http://stub", token="t")
    captured: List[Dict[str, Any]] = []

    def fake_request(method: str, url: str, **kwargs: Any) -> FakeResponse:
        captured.append({"method": method, "url": url, **kwargs})
        return handler(method, url, **kwargs)

    c._session.request = MagicMock(side_effect=fake_request)  # type: ignore[method-assign]
    return c, captured


# ---- Sandbox with int snapshot id --------------------------------------------


def test_sandbox_int_snapshot_forks_once_and_reuses_vm() -> None:
    """`with client.sandbox(42)` should:
    1. POST /v1/snapshots/42/fork exactly once on enter
    2. Route every subsequent exec through /v1/vms/<returned-id>/exec
       (i.e. the SAME VM handle)
    3. DELETE /v1/vms/<id> on exit
    """
    fork_response = {
        "vm": {"id": 101, "display": "vm-101", "state": "running"},
        "fork_ms": 12,
        "fork_count": 1,
        "fork_total_ms": 12,
    }

    def handler(method: str, url: str, **kwargs: Any) -> FakeResponse:
        if url.endswith("/v1/snapshots/42/fork"):
            return FakeResponse(200, fork_response)
        if url.endswith("/v1/vms/101/exec"):
            return FakeResponse(
                200,
                {"stdout": "ok\n", "stderr": "", "exit_code": 0, "duration_ms": 3},
            )
        if url.endswith("/v1/vms/101") and method == "DELETE":
            return FakeResponse(204, {})
        raise AssertionError(f"unexpected request: {method} {url}")

    c, captured = _scripted_client(handler)
    with c.sandbox(snapshot=42) as sb:
        r1 = sb.execute_python("print(1)")
        r2 = sb.execute_python("print(2)")
        assert r1.stdout == "ok\n"
        assert r2.stdout == "ok\n"

    # Exactly one fork call, two exec calls on the SAME vm id, one destroy.
    method_url = [(c["method"], c["url"]) for c in captured]
    assert method_url == [
        ("POST", "http://stub/v1/snapshots/42/fork"),
        ("POST", "http://stub/v1/vms/101/exec"),
        ("POST", "http://stub/v1/vms/101/exec"),
        ("DELETE", "http://stub/v1/vms/101"),
    ]


def test_sandbox_marketplace_string_forks_marketplace_endpoint() -> None:
    """`with client.sandbox("python-3.12-ds")` posts to the marketplace
    endpoint on enter, then reuses the returned VM."""

    def handler(method: str, url: str, **kwargs: Any) -> FakeResponse:
        if url.endswith("/v1/marketplace/snapshots/python-3.12-ds/fork"):
            return FakeResponse(
                200,
                {
                    "vm": {"id": 55, "display": "vm-55", "state": "running"},
                    "fork_ms": 12,
                    "fork_count": 1,
                    "fork_total_ms": 12,
                },
            )
        if url.endswith("/v1/vms/55/exec"):
            return FakeResponse(
                200,
                {"stdout": "pandas 2.2.3\n", "stderr": "", "exit_code": 0, "duration_ms": 4},
            )
        if url.endswith("/v1/vms/55") and method == "DELETE":
            return FakeResponse(204, {})
        raise AssertionError(f"unexpected: {method} {url}")

    c, captured = _scripted_client(handler)
    with c.sandbox(snapshot="python-3.12-ds") as sb:
        r = sb.execute_python("import pandas; print(pandas.__version__)")
        assert "pandas" in r.stdout

    assert captured[0]["url"].endswith("/v1/marketplace/snapshots/python-3.12-ds/fork")
    assert captured[-1]["method"] == "DELETE"


def test_sandbox_marketplace_name_is_url_encoded() -> None:
    """A name with reserved path characters should be
    passed through `quote(safe="")` so no reserved chars leak into
    the path."""
    captured_url: Dict[str, str] = {}

    def handler(method: str, url: str, **kwargs: Any) -> FakeResponse:
        if "/marketplace/snapshots/" in url and url.endswith("/fork"):
            captured_url["url"] = url
            return FakeResponse(
                200,
                {
                    "vm": {"id": 1, "display": "vm-1", "state": "running"},
                    "fork_ms": 12,
                    "fork_count": 1,
                    "fork_total_ms": 12,
                },
            )
        if url.endswith("/v1/vms/1") and method == "DELETE":
            return FakeResponse(204, {})
        raise AssertionError(f"unexpected: {method} {url}")

    c, _ = _scripted_client(handler)
    with c.sandbox(snapshot="weird/name?with&chars"):
        pass
    # `/`, `?`, `&` all get percent-encoded — the whole name is one
    # path segment, not sub-paths.
    assert captured_url["url"].endswith("weird%2Fname%3Fwith%26chars/fork")


def test_sandbox_bool_snapshot_is_rejected() -> None:
    c = nanovm.Client("http://stub", token="t")

    with pytest.raises(TypeError, match="snapshot must be int"):
        c.sandbox(snapshot=True).open()


def test_sandbox_destroys_vm_on_exception() -> None:
    """If the body of the `with` raises, __exit__ still destroys the VM."""

    def handler(method: str, url: str, **kwargs: Any) -> FakeResponse:
        if url.endswith("/v1/snapshots/1/fork"):
            return FakeResponse(
                200,
                {
                    "vm": {"id": 9, "display": "vm-9", "state": "running"},
                    "fork_ms": 12,
                    "fork_count": 1,
                    "fork_total_ms": 12,
                },
            )
        if url.endswith("/v1/vms/9") and method == "DELETE":
            return FakeResponse(204, {})
        raise AssertionError(f"unexpected: {method} {url}")

    c, captured = _scripted_client(handler)
    with pytest.raises(RuntimeError, match="user error"):
        with c.sandbox(snapshot=1):
            raise RuntimeError("user error")
    # Fork + destroy both happened.
    assert captured[0]["url"].endswith("/v1/snapshots/1/fork")
    assert captured[-1] == {
        "method": "DELETE",
        "url": "http://stub/v1/vms/9",
        "json": None,
        "timeout": 30.0,
    }


def test_sandbox_vm_property_errors_before_open() -> None:
    """Accessing `.vm` outside `with` (or before `.open()`) should
    raise a clear error rather than return `None`."""
    c = nanovm.Client("http://stub", token="t")
    sb = c.sandbox(snapshot=1)
    with pytest.raises(nanovm.NanovmError) as ei:
        _ = sb.vm
    assert ei.value.code == "sandbox_not_open"


# ---- fork_marketplace one-shot -----------------------------------------------


def test_fork_marketplace_returns_vm_directly() -> None:
    def handler(method: str, url: str, **kwargs: Any) -> FakeResponse:
        if url.endswith("/v1/marketplace/snapshots/alpine-3.20-shell/fork"):
            return FakeResponse(
                200,
                {
                    "vm": {"id": 77, "display": "vm-77", "state": "running"},
                    "fork_ms": 12,
                    "fork_count": 1,
                    "fork_total_ms": 12,
                },
            )
        raise AssertionError(f"unexpected: {method} {url}")

    c, _ = _scripted_client(handler)
    vm = c.fork_marketplace("alpine-3.20-shell")
    assert isinstance(vm, nanovm.Vm)
    assert vm.id == 77
    assert vm.state == "running"


# ---- PaymentRequiredError typed exception ------------------------------------


def test_402_raises_payment_required_with_upgrade_endpoint() -> None:
    def handler(method: str, url: str, **kwargs: Any) -> FakeResponse:
        return FakeResponse(
            402,
            {
                "error": {
                    "code": "subscription_delinquent",
                    "message": "subscription is past_due (since 2026-07-15T04:12:33.000Z, past the 72-hour grace window); resolve via the billing portal",
                    "upgrade_endpoint": "/v1/billing/portal",
                }
            },
        )

    c, _ = _scripted_client(handler)
    with pytest.raises(nanovm.PaymentRequiredError) as ei:
        c.execute_python("print(1)", snapshot=1)
    err = ei.value
    assert err.code == "subscription_delinquent"
    assert err.status == 402
    assert err.upgrade_endpoint == "/v1/billing/portal"
    assert "past_due" in str(err)


def test_402_without_upgrade_endpoint_still_raises_typed() -> None:
    """A minimalist 402 without the extended envelope still raises
    `PaymentRequiredError` — the field is just `None`."""

    def handler(method: str, url: str, **kwargs: Any) -> FakeResponse:
        return FakeResponse(
            402,
            {"error": {"code": "payment_required", "message": "please pay"}},
        )

    c, _ = _scripted_client(handler)
    with pytest.raises(nanovm.PaymentRequiredError) as ei:
        c.execute_python("print(1)", snapshot=1)
    assert ei.value.upgrade_endpoint is None


# ---- request-id surfacing on 5xx ---------------------------------------------


def test_5xx_error_message_includes_request_id_when_present() -> None:
    def handler(method: str, url: str, **kwargs: Any) -> FakeResponse:
        return FakeResponse(
            500,
            {"error": {"code": "internal", "message": "boom"}},
            headers={"x-request-id": "abcd1234"},
        )

    c, _ = _scripted_client(handler)
    with pytest.raises(nanovm.NanovmError) as ei:
        c.execute_python("print(1)", snapshot=1)
    assert "request_id=abcd1234" in str(ei.value)


def test_5xx_error_without_request_id_omits_suffix() -> None:
    def handler(method: str, url: str, **kwargs: Any) -> FakeResponse:
        return FakeResponse(500, {"error": {"code": "internal", "message": "boom"}})

    c, _ = _scripted_client(handler)
    with pytest.raises(nanovm.NanovmError) as ei:
        c.execute_python("print(1)", snapshot=1)
    assert "request_id" not in str(ei.value)
