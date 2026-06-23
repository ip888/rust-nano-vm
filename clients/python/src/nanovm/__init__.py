"""Python client for the rust-nano-vm control plane.

A thin synchronous wrapper around the REST surface documented in
``docs/openapi.json``. Designed to read like the lifecycle a user
actually drives:

    import nanovm

    client = nanovm.Client("http://localhost:8080", token="dev-token")

    vm = client.create_vm()
    vm.start()
    result = vm.exec(program="python3", args=["-c", "print(1+1)"])
    print(result.stdout)        # "2\n"
    print(result.exit_code)     # 0
    vm.destroy()

Snapshot + fork is a first-class primitive:

    snap = vm.snapshot()
    child = snap.fork()         # new VM, ~12 ms on real KVM

Errors are mapped to typed exceptions (``AuthError``, ``NotFoundError``,
``ConflictError``, ``RateLimited``) so callers can match on intent
rather than parsing the HTTP status code.

The client carries one ``requests.Session`` for connection reuse. It
is **not** thread-safe — wrap in a lock or build one client per thread.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional
from urllib.parse import urlencode

import requests

__version__ = "0.1.0"

__all__ = [
    "Client",
    "Vm",
    "Snapshot",
    "ExecResult",
    "Usage",
    "Health",
    "NanovmError",
    "AuthError",
    "NotFoundError",
    "ConflictError",
    "RateLimited",
    "__version__",
]


# ---------------------------------------------------------------------------
# Exceptions
# ---------------------------------------------------------------------------


class NanovmError(Exception):
    """Base exception for every client-side error.

    Carries the stable ``code`` string from the server's structured error
    envelope (e.g. ``"unknown_vm"``, ``"invalid_transition"``) plus the
    HTTP status. Callers should match on ``code`` rather than ``message``;
    the message is free to change between releases.
    """

    def __init__(
        self,
        message: str,
        code: Optional[str] = None,
        status: Optional[int] = None,
    ):
        super().__init__(message)
        self.code = code
        self.status = status


class AuthError(NanovmError):
    """Raised on 401: missing or invalid bearer token."""


class NotFoundError(NanovmError):
    """Raised on 404: unknown VM or snapshot id."""


class ConflictError(NanovmError):
    """Raised on 409: invalid state transition (e.g. start an already-running VM)."""


class RateLimited(NanovmError):
    """Raised on 429: per-token fork quota exhausted.

    ``retry_after`` is the integer seconds the server's Retry-After
    header asks the caller to wait before retrying.
    """

    def __init__(self, message: str, retry_after: int):
        super().__init__(message, code="too_many_requests", status=429)
        self.retry_after = retry_after


# ---------------------------------------------------------------------------
# DTOs
# ---------------------------------------------------------------------------


@dataclass
class ExecResult:
    """Outcome of ``Vm.exec``."""

    stdout: str
    stderr: str
    exit_code: Optional[int]
    signal: Optional[int]
    duration_ms: int


@dataclass
class Usage:
    """Caller's per-token fork-usage counters."""

    token: str
    fork_count: int
    fork_total_ms: int


@dataclass
class Health:
    """Structured health surface from ``GET /v1/health``."""

    ok: bool
    backend: str
    version: str
    uptime_secs: int
    started_at: str


@dataclass
class Vm:
    """A microVM. Hands back to the originating ``Client`` for all RPCs."""

    id: int
    display: str
    state: str
    # `repr=False` so `print(vm)` doesn't dump the whole client.
    _client: "Client" = field(repr=False, default=None)  # type: ignore[assignment]

    def start(self) -> None:
        self._client._request("POST", f"/v1/vms/{self.id}/start")
        self.state = "running"

    def stop(self) -> None:
        self._client._request("POST", f"/v1/vms/{self.id}/stop")
        self.state = "stopped"

    def destroy(self) -> None:
        self._client._request("DELETE", f"/v1/vms/{self.id}")

    def snapshot(self, to_dir: Optional[str] = None) -> "Snapshot":
        body: Dict[str, Any] = {}
        if to_dir is not None:
            body["to_dir"] = to_dir
        resp = self._client._request("POST", f"/v1/vms/{self.id}/snapshot", body=body or None)
        return Snapshot(id=resp["id"], display=resp["display"], _client=self._client)

    def exec(
        self,
        program: str,
        args: Optional[List[str]] = None,
        env: Optional[List[List[str]]] = None,
        cwd: Optional[str] = None,
        timeout_ms: Optional[int] = None,
    ) -> ExecResult:
        """Run ``program`` (with optional ``args``) inside the guest.

        ``env`` is a list of ``[KEY, VALUE]`` pairs to add to the guest
        process's environment. ``timeout_ms`` is the per-call deadline;
        ``None`` lets the server pick its default (currently 30 s).
        """
        body: Dict[str, Any] = {"program": program}
        if args is not None:
            body["args"] = list(args)
        if env is not None:
            body["env"] = [list(pair) for pair in env]
        if cwd is not None:
            body["cwd"] = cwd
        if timeout_ms is not None:
            body["timeout_ms"] = int(timeout_ms)
        resp = self._client._request("POST", f"/v1/vms/{self.id}/exec", body=body)
        return ExecResult(
            stdout=resp.get("stdout", ""),
            stderr=resp.get("stderr", ""),
            exit_code=resp.get("exit_code"),
            signal=resp.get("signal"),
            duration_ms=int(resp.get("duration_ms", 0)),
        )


@dataclass
class Snapshot:
    """A captured VM snapshot."""

    id: int
    display: str
    _client: "Client" = field(repr=False, default=None)  # type: ignore[assignment]

    def fork(self) -> Vm:
        """Cheap CoW fork — the headline product call.

        ``fork`` is metered separately from ``restore``: subject to the
        per-token fork-bucket quota. Raises ``RateLimited`` on 429.
        """
        resp = self._client._request("POST", f"/v1/snapshots/{self.id}/fork")
        vm = resp["vm"]
        return Vm(id=vm["id"], display=vm["display"], state=vm["state"], _client=self._client)

    def restore(self) -> Vm:
        """Restore a fresh VM from this snapshot. Unmetered.

        ``fork`` is what customer eval loops should use; ``restore`` is
        the unmetered form for internal operations.
        """
        resp = self._client._request("POST", f"/v1/snapshots/{self.id}/restore")
        return Vm(
            id=resp["id"],
            display=resp["display"],
            state=resp["state"],
            _client=self._client,
        )

    def delete(self) -> None:
        self._client._request("DELETE", f"/v1/snapshots/{self.id}")


# ---------------------------------------------------------------------------
# Client
# ---------------------------------------------------------------------------


class Client:
    """Synchronous client for the rust-nano-vm control plane.

    One client owns a ``requests.Session`` for connection reuse, so prefer
    instantiating once and passing it around. Not thread-safe; build one
    client per thread (or wrap in a lock).
    """

    def __init__(
        self,
        base_url: str,
        token: Optional[str] = None,
        timeout: float = 30.0,
        request_id: Optional[str] = None,
    ):
        """
        :param base_url: e.g. ``"http://localhost:8080"``.
        :param token:    bearer token; omit when the server has auth disabled.
        :param timeout:  per-request HTTP timeout in seconds. Note that
                         the server's own exec timeout is independent
                         (passed via ``Vm.exec(timeout_ms=...)``).
        :param request_id: optional fixed ``X-Request-Id`` to send on every
                         call (useful for correlation in batched workflows).
                         When ``None`` the server mints one per request.
        """
        self.base_url = base_url.rstrip("/")
        self.token = token
        self.timeout = float(timeout)
        self.request_id = request_id
        self._session = requests.Session()
        self._session.headers["User-Agent"] = f"nanovm-python/{__version__}"
        if token:
            self._session.headers["Authorization"] = f"Bearer {token}"
        if request_id:
            self._session.headers["X-Request-Id"] = request_id

    # -- low-level helpers --------------------------------------------------

    def _request(
        self,
        method: str,
        path: str,
        body: Optional[Dict[str, Any]] = None,
    ) -> Any:
        url = f"{self.base_url}{path}"
        try:
            resp = self._session.request(
                method, url, json=body, timeout=self.timeout
            )
        except requests.RequestException as e:
            raise NanovmError(f"transport error talking to {self.base_url}: {e}") from e
        if 200 <= resp.status_code < 300:
            if not resp.content:
                return None
            try:
                return resp.json()
            except ValueError:
                return resp.text
        # Error path — parse the structured envelope, fall back to raw text.
        code: str = "unknown"
        message: str = resp.text or f"HTTP {resp.status_code}"
        try:
            envelope = resp.json()
            err = envelope.get("error", {})
            code = err.get("code", code)
            message = err.get("message", message)
        except ValueError:
            pass
        if resp.status_code == 401:
            raise AuthError(message, code=code, status=401)
        if resp.status_code == 404:
            raise NotFoundError(message, code=code, status=404)
        if resp.status_code == 409:
            raise ConflictError(message, code=code, status=409)
        if resp.status_code == 429:
            retry_after = int(resp.headers.get("retry-after", "1") or "1")
            raise RateLimited(message, retry_after=retry_after)
        raise NanovmError(
            f"HTTP {resp.status_code} [{code}]: {message}",
            code=code,
            status=resp.status_code,
        )

    # -- high-level API -----------------------------------------------------

    def create_vm(
        self,
        vcpus: Optional[int] = None,
        memory_mib: Optional[int] = None,
        kernel: Optional[str] = None,
        rootfs: Optional[str] = None,
        cmdline: Optional[str] = None,
        vsock_cid: Optional[int] = None,
        snapshot_dir: Optional[str] = None,
    ) -> Vm:
        """Create a fresh VM. Returns a ``Vm`` you can ``start()``.

        All parameters are optional; the server uses sane defaults
        (1 vCPU, 128 MiB) when omitted.
        """
        body: Dict[str, Any] = {}
        if vcpus is not None:
            body["vcpus"] = int(vcpus)
        if memory_mib is not None:
            body["memory_mib"] = int(memory_mib)
        if kernel is not None:
            body["kernel"] = kernel
        if rootfs is not None:
            body["rootfs"] = rootfs
        if cmdline is not None:
            body["cmdline"] = cmdline
        if vsock_cid is not None:
            body["vsock_cid"] = int(vsock_cid)
        if snapshot_dir is not None:
            body["snapshot_dir"] = snapshot_dir
        resp = self._request("POST", "/v1/vms", body=body)
        return Vm(id=resp["id"], display=resp["display"], state=resp["state"], _client=self)

    def get_vm(self, vm_id: int) -> Vm:
        resp = self._request("GET", f"/v1/vms/{vm_id}")
        return Vm(id=resp["id"], display=resp["display"], state=resp["state"], _client=self)

    def list_vms(
        self, limit: Optional[int] = None, after: Optional[int] = None
    ) -> List[Vm]:
        """List VMs in id order. Walks one page; see ``iter_vms`` for
        the cursor-paginating helper.
        """
        params: Dict[str, Any] = {}
        if limit is not None:
            params["limit"] = limit
        if after is not None:
            params["after"] = after
        path = "/v1/vms" + (f"?{urlencode(params)}" if params else "")
        resp = self._request("GET", path)
        return [
            Vm(id=v["id"], display=v["display"], state=v["state"], _client=self)
            for v in resp.get("vms", [])
        ]

    def iter_vms(self, page_size: int = 100):
        """Yield every VM, transparently paginating via the ``next`` cursor.

        Use when the eval pipeline has accumulated thousands of forks
        and you don't want a 50 MiB response body.
        """
        after: Optional[int] = None
        while True:
            params: Dict[str, Any] = {"limit": page_size}
            if after is not None:
                params["after"] = after
            resp = self._request("GET", f"/v1/vms?{urlencode(params)}")
            for v in resp.get("vms", []):
                yield Vm(id=v["id"], display=v["display"], state=v["state"], _client=self)
            after = resp.get("next")
            if after is None:
                break

    def list_snapshots(
        self, limit: Optional[int] = None, after: Optional[int] = None
    ) -> List[Snapshot]:
        params: Dict[str, Any] = {}
        if limit is not None:
            params["limit"] = limit
        if after is not None:
            params["after"] = after
        path = "/v1/snapshots" + (f"?{urlencode(params)}" if params else "")
        resp = self._request("GET", path)
        return [
            Snapshot(id=s["id"], display=s["display"], _client=self)
            for s in resp.get("snapshots", [])
        ]

    def health(self) -> Health:
        """Structured health detail. Requires auth; for liveness use
        ``GET /healthz`` directly.
        """
        resp = self._request("GET", "/v1/health")
        return Health(
            ok=bool(resp["ok"]),
            backend=resp["backend"],
            version=resp["version"],
            uptime_secs=int(resp["uptime_secs"]),
            started_at=resp["started_at"],
        )

    def usage(self) -> Usage:
        """Caller's per-token fork counters."""
        resp = self._request("GET", "/v1/usage")
        return Usage(
            token=resp["token"],
            fork_count=int(resp["fork_count"]),
            fork_total_ms=int(resp["fork_total_ms"]),
        )

    def close(self) -> None:
        """Release the underlying connection pool."""
        self._session.close()

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *_: Any) -> None:
        self.close()
