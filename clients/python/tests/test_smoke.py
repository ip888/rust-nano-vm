"""End-to-end smoke tests for the nanovm Python SDK.

Run against a live control plane:

    docker run -d --rm -p 8080:8080 -e NANOVM_API_TOKENS=dev-token \\
        ghcr.io/ip888/nanovm-control-plane:latest

    pip install -e ./clients/python[dev]
    pytest clients/python/tests/

The test module skips itself when no control plane is reachable, so
running it in CI without a server up doesn't fail — same shape as the
``vm-kvm`` ``--features kvm`` integration tests.

These tests deliberately do NOT exercise ``Vm.exec`` against a real
guest: the published Docker image runs the mock backend, which
returns a canned exec response. Real exec is covered by the Rust-side
``exec_python_boot`` integration test.
"""

from __future__ import annotations

import os

import pytest
import requests

import nanovm


BASE_URL = os.environ.get("NANOVM_BASE_URL", "http://localhost:8080")
TOKEN = os.environ.get("NANOVM_API_TOKEN", "dev-token")


def _server_up() -> bool:
    try:
        r = requests.get(f"{BASE_URL}/healthz", timeout=2)
        return r.status_code == 200
    except requests.RequestException:
        return False


pytestmark = pytest.mark.skipif(
    not _server_up(),
    reason=(
        f"no control plane reachable at {BASE_URL}. "
        "Start one with: "
        "`docker run -d --rm -p 8080:8080 -e NANOVM_API_TOKENS=dev-token "
        "ghcr.io/ip888/nanovm-control-plane:latest`"
    ),
)


@pytest.fixture
def client() -> nanovm.Client:
    with nanovm.Client(BASE_URL, token=TOKEN) as c:
        yield c


# --- transport ---------------------------------------------------------------


def test_health_returns_structured_detail(client: nanovm.Client) -> None:
    h = client.health()
    assert h.ok is True
    assert h.backend in {"mock", "kvm"}
    # version_string follows semver — at minimum "<major>.<minor>.<patch>"
    parts = h.version.split(".")
    assert len(parts) == 3
    assert all(p.isdigit() for p in parts), f"non-numeric version: {h.version}"
    assert h.uptime_secs >= 0
    assert h.started_at.endswith("Z")


def test_auth_error_on_bad_token() -> None:
    with nanovm.Client(BASE_URL, token="definitely-not-the-real-token") as bad:
        with pytest.raises(nanovm.AuthError) as excinfo:
            bad.create_vm()
        assert excinfo.value.status == 401
        assert excinfo.value.code == "unauthorized"


# --- VM lifecycle ------------------------------------------------------------


def test_create_start_destroy_roundtrip(client: nanovm.Client) -> None:
    vm = client.create_vm()
    assert vm.id > 0
    assert vm.state == "created"

    vm.start()
    # GET back the state and confirm
    refetched = client.get_vm(vm.id)
    assert refetched.state == "running"

    vm.destroy()
    with pytest.raises(nanovm.NotFoundError):
        client.get_vm(vm.id)


def test_double_start_raises_conflict(client: nanovm.Client) -> None:
    vm = client.create_vm()
    vm.start()
    try:
        with pytest.raises(nanovm.ConflictError) as excinfo:
            vm.start()
        assert excinfo.value.status == 409
        assert excinfo.value.code == "invalid_transition"
    finally:
        vm.destroy()


# --- snapshot + fork ---------------------------------------------------------


def test_snapshot_then_fork_yields_new_vm(client: nanovm.Client) -> None:
    base = client.create_vm()
    base.start()
    try:
        snap = base.snapshot()
        assert snap.id > 0

        child = snap.fork()
        try:
            assert child.id != base.id
            # A fork inherits the snapshotted VM's lifecycle state. Since
            # we snapshot a running VM (snapshot of a stopped VM is also
            # legal, just not what eval pipelines do), the child comes
            # back already running. Asserting "running" rather than
            # "created" is the right shape — it'd be a regression if a
            # fork came back needing an explicit `.start()`.
            assert child.state == "running"
        finally:
            child.destroy()
    finally:
        base.destroy()


def test_usage_increments_on_fork(client: nanovm.Client) -> None:
    base = client.create_vm()
    base.start()
    try:
        snap = base.snapshot()
        u0 = client.usage()
        child = snap.fork()
        try:
            u1 = client.usage()
            assert u1.fork_count == u0.fork_count + 1
            assert u1.token.startswith("tok-")
        finally:
            child.destroy()
    finally:
        base.destroy()


# --- pagination --------------------------------------------------------------


def test_list_vms_default_caps_response(client: nanovm.Client) -> None:
    """A `?limit` of None defaults to 100; the listing must respect it."""
    page = client.list_vms()  # default
    assert isinstance(page, list)
    assert len(page) <= 100


def test_iter_vms_visits_at_least_one_seeded(client: nanovm.Client) -> None:
    vm = client.create_vm()
    seen = False
    try:
        for v in client.iter_vms(page_size=50):
            if v.id == vm.id:
                seen = True
                break
        assert seen, f"iter_vms didn't yield seeded VM id={vm.id}"
    finally:
        vm.destroy()


def test_list_vms_zero_limit_is_bad_request(client: nanovm.Client) -> None:
    with pytest.raises(nanovm.NanovmError) as excinfo:
        client.list_vms(limit=0)
    assert excinfo.value.status == 400
    assert excinfo.value.code == "bad_request"
