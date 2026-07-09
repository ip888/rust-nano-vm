"""Async client for the rust-nano-vm control plane.

Complements the synchronous :class:`nanovm.Client` for callers who
want to drive the control plane from ``asyncio`` — LangChain, OpenAI
Agents SDK, CrewAI's async workflows, or a FastAPI backend.

Public surface mirrors :class:`nanovm.Client` but every method is a
coroutine::

    import asyncio, nanovm

    async def main():
        client = nanovm.AsyncClient("http://localhost:8080", token="dev-token")
        try:
            result = await client.execute_python("print(1 + 1)")
            print(result.stdout)   # "2\n"
        finally:
            await client.aclose()

    asyncio.run(main())

For agent-framework loops you'll usually want ``async with``::

    async with nanovm.AsyncClient(...) as client:
        for prompt in prompts:
            result = await client.execute_python(prompt.code)
            ...

## Retries

Transient failures — 429 (rate-limited by the fork quota), 502/503/504
(the control plane restarting, a KVM worker crashed and is being
respawned), and network hiccups — are retried with exponential backoff
capped at ``max_retries`` (default 5). A 429 that carries a
``Retry-After`` header waits that long instead. Terminal errors
(401 auth, 404 not found, 400 bad request) short-circuit immediately.

## Why a separate module

The sync ``Client`` uses ``requests``, which has no async story. Rather
than force every user to install ``httpx`` up front, this module is
behind the ``nanovm[async]`` extra::

    pip install "nanovm[async]"

Users who only need the sync surface don't pay the httpx install cost.
"""

from __future__ import annotations

import asyncio
import random
from dataclasses import dataclass
from typing import Any, Dict, Optional

try:
    import httpx
except ImportError as e:  # pragma: no cover
    raise ImportError(
        "nanovm.aio requires 'httpx'. Install with: pip install 'nanovm[async]'"
    ) from e

from . import (
    AuthError,
    ConflictError,
    Health,
    NanovmError,
    NotFoundError,
    RateLimited,
    SandboxResult,
)

__all__ = ["AsyncClient"]


# ---- Retry policy ---------------------------------------------------

# HTTP status codes that indicate a transient failure the client should
# retry (rather than surfacing as an immediate error to the caller).
# 429 = rate-limited; 502/503/504 = a proxy or the control plane
# restarting; 500 kept OUT because it usually indicates a real bug the
# retry won't paper over.
_RETRIABLE_STATUSES = frozenset({429, 502, 503, 504})

# httpx exceptions that mean "the connection didn't complete" and are
# safe to retry (as opposed to `HTTPStatusError` which we handle above).
_RETRIABLE_EXCEPTIONS = (
    httpx.ConnectError,
    httpx.ReadTimeout,
    httpx.WriteTimeout,
    httpx.PoolTimeout,
    httpx.RemoteProtocolError,
)


def _backoff(attempt: int, base: float = 0.25, cap: float = 8.0) -> float:
    """Full-jitter exponential backoff — attempt 0 sleeps up to 0.25 s,
    attempt 3 sleeps up to 2 s, etc. Randomising the wait spreads a
    thundering-herd retry storm from a fleet of workers all synced
    on the same 429.
    """
    upper = min(cap, base * (2**attempt))
    return random.uniform(0, upper)


# ---- Client ---------------------------------------------------------


class AsyncClient:
    """Async counterpart to :class:`nanovm.Client`. Thread-agnostic —
    every method returns a coroutine that must be awaited on the same
    event loop the client was created on.

    Parameters
    ----------
    base_url:
        Root URL of the control plane, e.g. ``http://localhost:8080``.
    token:
        Bearer token for ``Authorization: Bearer <token>``. Pass ``None``
        for a demo deployment running with ``NANOVM_API_TOKENS`` unset
        (dev-only).
    timeout_s:
        Per-request timeout in seconds. Defaults to 30 s — long enough
        for a cold-start fork on a busy KVM host.
    max_retries:
        Number of retries on transient errors (see module docstring).
        ``0`` disables retries entirely (useful when the caller wants
        to drive its own retry loop).
    """

    def __init__(
        self,
        base_url: str,
        token: Optional[str] = None,
        *,
        timeout_s: float = 30.0,
        max_retries: int = 5,
        transport: Optional[httpx.AsyncBaseTransport] = None,
    ) -> None:
        self._base_url = base_url.rstrip("/")
        self._token = token
        self._max_retries = max_retries
        headers: Dict[str, str] = {"accept": "application/json"}
        if token:
            headers["authorization"] = f"Bearer {token}"
        # Always own the underlying httpx client so aclose() reliably
        # tears down the connection pool. Tests substitute a
        # `MockTransport`; production leaves `transport=None`.
        self._client = httpx.AsyncClient(
            base_url=self._base_url,
            headers=headers,
            timeout=timeout_s,
            transport=transport,
        )

    async def __aenter__(self) -> "AsyncClient":
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        await self.aclose()

    async def aclose(self) -> None:
        """Release the underlying HTTP connection pool. Idempotent."""
        await self._client.aclose()

    # ---- Public API --------------------------------------------------

    async def health(self) -> Health:
        """Fetch structured health from ``GET /v1/health``."""
        body = await self._request("GET", "/v1/health")
        return Health(
            ok=bool(body.get("ok", False)),
            backend=str(body.get("backend", "")),
            version=str(body.get("version", "")),
            uptime_secs=int(body.get("uptime_secs", 0)),
            started_at=str(body.get("started_at", "")),
        )

    async def sandbox_invoke(
        self,
        action: str,
        snapshot: Optional[int] = None,
        **action_args: Any,
    ) -> SandboxResult:
        """Async ``POST /v1/sandbox/invoke``.

        See :meth:`nanovm.Client.sandbox_invoke` for the semantic
        contract — this is a byte-identical wrapper.
        """
        payload: Dict[str, Any] = {"action": action}
        if snapshot is not None:
            payload["snapshot"] = int(snapshot)
        payload.update(action_args)
        body = await self._request("POST", "/v1/sandbox/invoke", json=payload)
        return SandboxResult(
            stdout=body.get("stdout", ""),
            stderr=body.get("stderr", ""),
            exit_code=int(body.get("exit_code", 0)),
            duration_ms=int(body.get("duration_ms", 0)),
            cold_start=bool(body.get("cold_start", False)),
        )

    async def execute_python(
        self,
        code: str,
        snapshot: Optional[int] = None,
        timeout_ms: Optional[int] = None,
    ) -> SandboxResult:
        """Run a Python program (``python3 -c <code>``) in a fresh
        sandbox VM. Returns the captured stdout / stderr / exit code.
        """
        kwargs: Dict[str, Any] = {"code": code}
        if timeout_ms is not None:
            kwargs["timeout_ms"] = int(timeout_ms)
        return await self.sandbox_invoke("execute_python", snapshot=snapshot, **kwargs)

    async def execute_shell(
        self,
        command: str,
        snapshot: Optional[int] = None,
        timeout_ms: Optional[int] = None,
    ) -> SandboxResult:
        """Run a shell command (``sh -c <command>``) in a fresh
        sandbox VM. Returns the captured stdout / stderr / exit code.
        """
        kwargs: Dict[str, Any] = {"command": command}
        if timeout_ms is not None:
            kwargs["timeout_ms"] = int(timeout_ms)
        return await self.sandbox_invoke("execute_shell", snapshot=snapshot, **kwargs)

    async def read_file(
        self,
        path: str,
        snapshot: Optional[int] = None,
    ) -> SandboxResult:
        """Read a file from the guest filesystem."""
        return await self.sandbox_invoke("read_file", snapshot=snapshot, path=path)

    async def write_file(
        self,
        path: str,
        content: str,
        snapshot: Optional[int] = None,
        mode: Optional[int] = None,
    ) -> SandboxResult:
        """Write a file to the guest filesystem."""
        kwargs: Dict[str, Any] = {"path": path, "content": content}
        if mode is not None:
            kwargs["mode"] = int(mode)
        return await self.sandbox_invoke("write_file", snapshot=snapshot, **kwargs)

    # ---- Internals ---------------------------------------------------

    async def _request(
        self,
        method: str,
        path: str,
        *,
        json: Optional[Dict[str, Any]] = None,
    ) -> Dict[str, Any]:
        """Send `method path` with retries; raise the right typed
        exception on terminal failures. Returns the parsed JSON body
        (dict) — endpoints that return non-JSON aren't reachable
        through this client today.
        """
        attempt = 0
        last_exc: Optional[BaseException] = None
        while True:
            try:
                resp = await self._client.request(method, path, json=json)
            except _RETRIABLE_EXCEPTIONS as exc:
                last_exc = exc
                if attempt >= self._max_retries:
                    raise NanovmError(
                        f"network error after {attempt} retries: {exc}"
                    ) from exc
                await asyncio.sleep(_backoff(attempt))
                attempt += 1
                continue

            if resp.status_code < 300:
                if resp.content:
                    return resp.json()
                return {}

            # 429 with Retry-After: honour it verbatim rather than the
            # backoff schedule. `Retry-After: 0` means "retry immediately",
            # which is a valid signal — don't fall through to backoff via
            # `or` (falsy) since that would over-wait. Only fall back
            # when the header is genuinely absent (parser returns None).
            if resp.status_code == 429 and attempt < self._max_retries:
                retry_after = _retry_after_seconds(resp)
                wait_s = retry_after if retry_after is not None else _backoff(attempt)
                await asyncio.sleep(wait_s)
                attempt += 1
                continue

            # Other transient statuses: exponential backoff.
            if resp.status_code in _RETRIABLE_STATUSES and attempt < self._max_retries:
                await asyncio.sleep(_backoff(attempt))
                attempt += 1
                continue

            # Terminal. Map to a typed exception.
            _raise_for_status(resp)
        # Unreachable — either return, retry, or raise above.

        raise NanovmError(  # pragma: no cover
            f"exhausted retries; last exception: {last_exc}"
        )


def _retry_after_seconds(resp: httpx.Response) -> Optional[float]:
    """Parse a ``Retry-After`` header value. Accepts the seconds form
    the control plane emits (``Retry-After: 1``); ignores HTTP-date
    variants (uncommon and not used here).
    """
    raw = resp.headers.get("retry-after")
    if not raw:
        return None
    try:
        return max(0.0, float(raw))
    except ValueError:
        return None


def _raise_for_status(resp: httpx.Response) -> None:
    """Map a non-2xx response to a typed :class:`NanovmError` subclass.
    Same shape as :meth:`nanovm.Client._raise_for_response`.

    The control-plane wraps errors as ``{"error": {"code": …, "message": …}}``
    (see ``crates/control-plane/src/error.rs``). Older revisions of this
    module read flat top-level ``code``/``message``, silently losing the
    structured error code on every 4xx/5xx. Both shapes are accepted here
    for forward-compat, but the nested envelope is the canonical form.
    """
    try:
        body = resp.json() if resp.content else {}
    except ValueError:
        body = {}
    envelope = body.get("error") if isinstance(body, dict) else None
    if isinstance(envelope, dict):
        code = envelope.get("code")
        message = envelope.get("message")
    else:
        # Legacy / flat shape: some future endpoints might emit it too.
        code = body.get("code") if isinstance(body, dict) else None
        message = body.get("message") if isinstance(body, dict) else None
    if not message:
        message = resp.text or f"HTTP {resp.status_code}"
    status = resp.status_code
    if status == 401:
        raise AuthError(message, code=code, status=status)
    if status == 404:
        raise NotFoundError(message, code=code, status=status)
    if status == 409:
        raise ConflictError(message, code=code, status=status)
    if status == 429:
        # `RateLimited`'s __init__ hardcodes `code="too_many_requests"`
        # and `status=429` in the sync client — match that shape so the
        # exception is indistinguishable from a sync-raised one. Default
        # `retry_after` to 1 (not 0) when the header is missing: 0 would
        # tell a caller-side retry loop to hammer immediately, which
        # defeats the throttle's purpose.
        retry_after_val = _retry_after_seconds(resp)
        retry_after = int(retry_after_val) if retry_after_val is not None else 1
        raise RateLimited(message, retry_after)
    raise NanovmError(message, code=code, status=status)


# Keep for symmetry with the sync module's dataclass export.
@dataclass
class _AsyncClientMarker:
    """Reserved for future async-only shapes."""

    pass
