"""Tests for :class:`nanovm.AsyncClient`. Driven against a stub HTTP
handler installed on the httpx client, so no network / control plane
is needed.

Run with::

    cd clients/python
    pip install -e '.[dev]'
    pytest tests/test_async_client.py -v
"""

from __future__ import annotations

import json

import httpx
import pytest

import nanovm
from nanovm import RateLimited, AuthError, NotFoundError


def _stub_transport(responses):
    """Build an httpx MockTransport that pops responses off the list on
    each request. `responses` is a list of ``(status, body_dict, headers?)``.
    """

    def handler(request: httpx.Request) -> httpx.Response:
        status, body, *rest = responses.pop(0)
        headers = rest[0] if rest else {}
        return httpx.Response(status, json=body, headers=headers)

    return httpx.MockTransport(handler)


def _client(responses, *, max_retries: int = 3):
    return nanovm.AsyncClient(
        base_url="http://stub",
        token="tok",
        transport=_stub_transport(responses),
        max_retries=max_retries,
    )


@pytest.mark.asyncio
async def test_execute_python_happy_path():
    responses = [
        (
            200,
            {"stdout": "hello\n", "stderr": "", "exit_code": 0, "duration_ms": 12},
        )
    ]
    async with _client(responses) as client:
        result = await client.execute_python("print('hello')")
    assert result.stdout == "hello\n"
    assert result.exit_code == 0
    assert result.duration_ms == 12


@pytest.mark.asyncio
async def test_health_typed_response():
    responses = [
        (
            200,
            {
                "ok": True,
                "backend": "kvm-fleet",
                "version": "0.0.3",
                "uptime_secs": 42,
                "started_at": "2026-07-01T00:00:00Z",
            },
        )
    ]
    async with _client(responses) as client:
        h = await client.health()
    assert h.ok is True
    assert h.backend == "kvm-fleet"


@pytest.mark.asyncio
async def test_429_retries_then_succeeds():
    responses = [
        (429, {"code": "quota_exceeded", "message": "rps ceiling"}, {"retry-after": "0"}),
        (429, {"code": "quota_exceeded", "message": "rps ceiling"}, {"retry-after": "0"}),
        (200, {"stdout": "ok\n", "stderr": "", "exit_code": 0, "duration_ms": 1}),
    ]
    async with _client(responses) as client:
        result = await client.execute_python("pass")
    assert result.stdout == "ok\n"


@pytest.mark.asyncio
async def test_429_with_no_more_retries_raises_rate_limited():
    # max_retries=0 → first 429 becomes terminal.
    async with _client(
        [(429, {"code": "quota_exceeded", "message": "over"}, {"retry-after": "3"})],
        max_retries=0,
    ) as client:
        with pytest.raises(RateLimited) as ex:
            await client.execute_python("pass")
    # Sync client hardcodes .code to "too_many_requests"; async matches.
    assert ex.value.retry_after == 3


@pytest.mark.asyncio
async def test_401_short_circuits_no_retry():
    responses = [
        (401, {"code": "unauthenticated", "message": "bad token"}),
        # If we retried, this second response would be consumed.
        # It isn't, so the assertion below (len == 1) proves we short-circuited.
        (200, {"stdout": "shouldnt-reach", "stderr": "", "exit_code": 0, "duration_ms": 0}),
    ]
    async with _client(responses) as client:
        with pytest.raises(AuthError):
            await client.execute_python("pass")
    # One response left over — proves we didn't retry.
    assert len(responses) == 1


@pytest.mark.asyncio
async def test_404_maps_to_not_found():
    responses = [(404, {"code": "unknown_snapshot", "message": "no such id"})]
    async with _client(responses) as client:
        with pytest.raises(NotFoundError):
            await client.execute_python("pass", snapshot=999)


@pytest.mark.asyncio
async def test_aclose_is_idempotent():
    """`aclose` must be safe to call twice — used by both `async with`
    exit and explicit teardown paths.
    """
    responses = [(200, {"stdout": "", "stderr": "", "exit_code": 0, "duration_ms": 0})]
    async with _client(responses) as client:
        await client.execute_python("pass")
        # First aclose here is via `async with` __aexit__ below.
    # Second aclose is explicit — must not raise.
    await client.aclose()


@pytest.mark.asyncio
async def test_sandbox_invoke_forwards_action_and_args():
    captured = {}

    def handler(request: httpx.Request) -> httpx.Response:
        captured["method"] = request.method
        captured["url"] = str(request.url)
        captured["body"] = json.loads(request.content.decode())
        return httpx.Response(
            200,
            json={"stdout": "", "stderr": "", "exit_code": 0, "duration_ms": 0},
        )

    transport = httpx.MockTransport(handler)
    async with nanovm.AsyncClient(
        base_url="http://stub", token="tok", transport=transport
    ) as client:
        await client.sandbox_invoke(
            "execute_python", snapshot=42, code="print(1)", timeout_ms=1000
        )
    assert captured["method"] == "POST"
    assert "/v1/sandbox/invoke" in captured["url"]
    assert captured["body"] == {
        "action": "execute_python",
        "snapshot": 42,
        "code": "print(1)",
        "timeout_ms": 1000,
    }
