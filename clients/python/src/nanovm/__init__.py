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

import base64
from dataclasses import dataclass, field
from typing import Any, Dict, Iterator, List, Optional, Union
from urllib.parse import urlencode

import requests

__version__ = "0.1.0"

__all__ = [
    "Client",
    "Vm",
    "Snapshot",
    "ExecResult",
    "ExecChunk",
    "ExecExit",
    "ExecStreamEvent",
    "SandboxResult",
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
class ExecChunk:
    """One chunk of stdout/stderr from ``Vm.exec_stream``.

    ``kind`` is ``"stdout"`` or ``"stderr"``. ``data`` is raw bytes —
    chunk boundaries follow the underlying SSE frames and may not
    align to lines or UTF-8 character boundaries.
    """

    kind: str
    data: bytes


@dataclass
class ExecExit:
    """Terminal event from ``Vm.exec_stream`` — process finished.

    Yielded exactly once, as the last item. After this, the iterator
    is exhausted.
    """

    exit_code: Optional[int]
    signal: Optional[int]
    duration_ms: int


# Type alias for the union of events ``exec_stream`` can yield.
ExecStreamEvent = Union[ExecChunk, ExecExit]


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
class SandboxResult:
    """Outcome of ``Client.sandbox_invoke`` and the action-specific
    convenience wrappers (``execute_python``, ``execute_shell``,
    ``read_file``, ``write_file``, ``list_files``).

    The fields mirror the server's flat envelope from
    ``POST /v1/sandbox/invoke``:

    - ``stdout`` / ``stderr`` — captured streams (UTF-8 lossy)
    - ``exit_code`` — POSIX-shell convention: signal-killed processes
      report ``128 + signal``. File-op actions report ``0`` on success.
    - ``duration_ms`` — wall-clock from snapshot-fork through destroy
    - ``cold_start`` — ``True`` iff the VM was cold-restored from the
      snapshot (``False`` on a warm-pool hit)
    """

    stdout: str
    stderr: str
    exit_code: int
    duration_ms: int
    cold_start: bool


def _iter_sse(resp: "requests.Response") -> Iterator[ExecStreamEvent]:
    """Parse the SSE body of `resp` into ExecChunk / ExecExit events.

    Generator: yields events as they arrive. Handles ``keep-alive``
    comment lines (lines starting with ``:``) by ignoring them. After
    yielding ``ExecExit``, closes the underlying response.
    """
    try:
        event_name: Optional[str] = None
        data_parts: List[str] = []

        def _emit(name: Optional[str], parts: List[str]) -> Optional[ExecStreamEvent]:
            if not name:
                return None
            # SSE concatenates multiple `data:` lines with newlines.
            data = "\n".join(parts)
            if name == "stdout" or name == "stderr":
                # Invalid base64 means the server's wire format is
                # broken (or something is rewriting bytes on the
                # path). Silently dropping the chunk would hide
                # protocol bugs; raise so callers see the problem.
                try:
                    raw = base64.b64decode(data, validate=True)
                except (ValueError, base64.binascii.Error) as e:
                    raise NanovmError(
                        f"malformed base64 in {name!r} SSE event: {e}",
                        code="bad_stream_payload",
                        status=0,
                    ) from e
                return ExecChunk(kind=name, data=raw)
            if name == "exit":
                # Malformed exit payload means we don't know what the
                # process did. Same logic: silently filling defaults
                # would let callers treat a corrupt frame as a clean
                # `exit_code=None` completion. Raise instead.
                import json as _json
                try:
                    payload = _json.loads(data) if data else {}
                except ValueError as e:
                    raise NanovmError(
                        f"malformed JSON in 'exit' SSE event: {e}",
                        code="bad_stream_payload",
                        status=0,
                    ) from e
                return ExecExit(
                    exit_code=payload.get("exit_code"),
                    signal=payload.get("signal"),
                    duration_ms=int(payload.get("duration_ms", 0)),
                )
            if name == "error":
                raise NanovmError(f"stream error: {data}", code="stream_error", status=0)
            # Unknown event names: tolerate quietly so future server
            # additions don't break old clients.
            return None

        for raw_line in resp.iter_lines(decode_unicode=False):
            # `iter_lines` yields ``None`` only for trailing bytes
            # without a terminator. SSE record boundary is a blank
            # line — represented as ``b""``.
            if raw_line is None:
                continue
            if raw_line == b"":
                ev = _emit(event_name, data_parts)
                event_name, data_parts = None, []
                if ev is not None:
                    yield ev
                    if isinstance(ev, ExecExit):
                        return
                continue
            line = raw_line.decode("utf-8", errors="replace")
            if line.startswith(":"):
                # Comment / keep-alive ping; ignore.
                continue
            if line.startswith("event:"):
                event_name = line[len("event:"):].strip()
            elif line.startswith("data:"):
                # SSE spec: data field strips a single leading space.
                rest = line[len("data:"):]
                if rest.startswith(" "):
                    rest = rest[1:]
                data_parts.append(rest)
        # Flush any trailing record without a terminating blank line.
        ev = _emit(event_name, data_parts)
        if ev is not None:
            yield ev
    finally:
        resp.close()


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

    def exec_stream(
        self,
        program: str,
        args: Optional[List[str]] = None,
        env: Optional[List[List[str]]] = None,
        cwd: Optional[str] = None,
        timeout_ms: Optional[int] = None,
    ) -> Iterator[ExecStreamEvent]:
        """Stream guest exec output via Server-Sent Events.

        Yields ``ExecChunk`` for each stdout/stderr fragment as it
        arrives, then exactly one terminal ``ExecExit``. The iterator
        ends after ``ExecExit``.

        Errors raised before the stream is established (``UnknownVm``,
        ``InvalidTransition``, auth) surface as the usual typed
        exceptions. Backend errors mid-stream surface as
        ``NanovmError`` raised from the iterator.

        Example::

            for event in vm.exec_stream("sh", ["-c", "echo hi"]):
                if isinstance(event, ExecChunk):
                    print(event.kind, event.data)
                else:
                    print("exit", event.exit_code)
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
        return self._client._sse_post(f"/v1/vms/{self.id}/exec/stream", body=body)


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
        # Error path — let the shared error mapper raise.
        self._raise_for_error(resp)
        # Unreachable — _raise_for_error always raises.
        return None  # pragma: no cover

    def _raise_for_error(self, resp: "requests.Response") -> None:
        """Map a non-2xx response to the right typed exception.

        Reused by both ``_request`` (one-shot JSON path) and
        ``_sse_post`` (streaming path — must surface errors that
        happen BEFORE the SSE stream opens, like ``404 unknown_vm``).
        """
        code = "unknown"
        message = resp.text or f"HTTP {resp.status_code}"
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

    def _sse_post(
        self, path: str, body: Optional[Dict[str, Any]] = None
    ) -> Iterator[ExecStreamEvent]:
        """POST a JSON body and stream the text/event-stream response.

        Returns a generator that yields ``ExecChunk`` / ``ExecExit``.
        Errors before the stream opens (4xx/5xx with a JSON envelope)
        raise the appropriate typed exception synchronously; errors
        once streaming has begun surface as ``NanovmError`` raised
        from inside the iterator.
        """
        url = f"{self.base_url}{path}"
        try:
            resp = self._session.post(
                url,
                json=body,
                stream=True,
                timeout=self.timeout,
                headers={"accept": "text/event-stream"},
            )
        except requests.RequestException as e:
            raise NanovmError(
                f"transport error talking to {self.base_url}: {e}"
            ) from e
        if resp.status_code >= 300:
            try:
                self._raise_for_error(resp)
            finally:
                resp.close()
        return _iter_sse(resp)

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

    # -- sandbox-action API ------------------------------------------------
    #
    # The control-plane's `POST /v1/sandbox/invoke` is a single endpoint
    # that does "fork from snapshot → run one action → destroy the VM →
    # return a flat envelope". It's the path AI-agent tool runners and
    # MCP servers should use — they don't want to manage the VM
    # lifecycle, they just want a sandboxed result.
    #
    # `sandbox_invoke` is the low-level escape hatch (passes the action
    # name as a string), and the five `execute_*` / `*_file` methods
    # below are typed convenience wrappers around it.

    def sandbox_invoke(
        self,
        action: str,
        snapshot: Optional[int] = None,
        **action_args: Any,
    ) -> SandboxResult:
        """Invoke an action against an ephemeral sandbox VM.

        ``action`` is one of ``"execute_python"``, ``"execute_shell"``,
        ``"read_file"``, ``"write_file"``, ``"list_files"``. The
        remaining keyword arguments are flattened into the request
        body alongside the discriminator — see the action-specific
        convenience methods for the expected field names.

        ``snapshot`` overrides the server's
        ``NANOVM_SANDBOX_SNAPSHOT_ID`` default. Passing both works:
        body wins.

        Raises ``NotFoundError`` when the snapshot is unknown,
        ``NanovmError`` (400) when no snapshot id is resolvable.
        """
        body: Dict[str, Any] = {"action": action}
        if snapshot is not None:
            body["snapshot"] = int(snapshot)
        body.update(action_args)
        resp = self._request("POST", "/v1/sandbox/invoke", body=body)
        return SandboxResult(
            stdout=resp.get("stdout", ""),
            stderr=resp.get("stderr", ""),
            exit_code=int(resp.get("exit_code", 0)),
            duration_ms=int(resp.get("duration_ms", 0)),
            cold_start=bool(resp.get("cold_start", False)),
        )

    def execute_python(
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
        return self.sandbox_invoke("execute_python", snapshot=snapshot, **kwargs)

    def execute_shell(
        self,
        command: str,
        snapshot: Optional[int] = None,
        timeout_ms: Optional[int] = None,
    ) -> SandboxResult:
        """Run a shell command (``sh -c <command>``) in a fresh
        sandbox VM.
        """
        kwargs: Dict[str, Any] = {"command": command}
        if timeout_ms is not None:
            kwargs["timeout_ms"] = int(timeout_ms)
        return self.sandbox_invoke("execute_shell", snapshot=snapshot, **kwargs)

    def read_file(
        self,
        path: str,
        snapshot: Optional[int] = None,
    ) -> SandboxResult:
        """Read a file from the guest filesystem.

        File content (UTF-8 lossy) lands in ``result.stdout``;
        ``exit_code`` is ``0`` on success. Missing-file / IO errors
        from the guest agent surface as ``NanovmError`` (5xx).
        """
        return self.sandbox_invoke("read_file", snapshot=snapshot, path=path)

    def write_file(
        self,
        path: str,
        content: str,
        mode: Optional[int] = None,
        snapshot: Optional[int] = None,
    ) -> SandboxResult:
        """Write a file to the guest filesystem.

        ``mode`` defaults to ``0o644`` server-side.
        ``result.stdout`` carries ``"bytes_written=N"`` on success.
        """
        kwargs: Dict[str, Any] = {"path": path, "content": content}
        if mode is not None:
            kwargs["mode"] = int(mode)
        return self.sandbox_invoke("write_file", snapshot=snapshot, **kwargs)

    def list_files(
        self,
        path: str,
        snapshot: Optional[int] = None,
    ) -> SandboxResult:
        """List directory entries (``ls -1 -- <path>``) in a fresh
        sandbox VM. One entry per line in ``result.stdout``.
        """
        return self.sandbox_invoke("list_files", snapshot=snapshot, path=path)

    def close(self) -> None:
        """Release the underlying connection pool."""
        self._session.close()

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *_: Any) -> None:
        self.close()
