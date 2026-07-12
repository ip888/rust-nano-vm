"""Tests for the ``nanovm`` shell command.

The CLI is a thin layer over the Client, so tests exercise the
argument parsing + config-file behaviour + exit codes rather than the
HTTP layer (which the SDK's own tests already cover). Every test
points NANOVM_CONFIG at a tempdir so nothing lands in
``~/.config/nanovm``.
"""

from __future__ import annotations

import io
import json
import os
import sys
from pathlib import Path
from typing import Iterator
from unittest.mock import patch

import pytest

from nanovm import cli


@pytest.fixture
def tmp_config(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    """Point NANOVM_CONFIG at a fresh file per test."""
    path = tmp_path / "config.json"
    monkeypatch.setenv("NANOVM_CONFIG", str(path))
    yield path


def _write_session(path: Path, *, api_url: str = "http://localhost:8080",
                   api_key: str = "acme:secret", org: str = "acme") -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"api_url": api_url, "api_key": api_key, "org": org}))


# -------- config helpers --------------------------------------------------


def test_config_path_respects_env_override(monkeypatch: pytest.MonkeyPatch,
                                            tmp_path: Path) -> None:
    override = tmp_path / "custom.json"
    monkeypatch.setenv("NANOVM_CONFIG", str(override))
    assert cli._config_path() == override


def test_load_session_returns_none_for_missing_file(tmp_config: Path) -> None:
    assert cli._load_session() is None


def test_load_session_returns_none_for_malformed_json(tmp_config: Path) -> None:
    tmp_config.write_text("not-json")
    assert cli._load_session() is None


def test_load_session_returns_none_for_missing_fields(tmp_config: Path) -> None:
    tmp_config.write_text(json.dumps({"api_url": "u"}))  # no api_key, no org
    assert cli._load_session() is None


def test_save_then_load_roundtrips(tmp_config: Path) -> None:
    cli._save_session(cli.Session(api_url="http://x", api_key="k", org="o"))
    session = cli._load_session()
    assert session is not None
    assert session.api_url == "http://x"
    assert session.api_key == "k"
    assert session.org == "o"


def test_save_sets_owner_only_perms_on_unix(tmp_config: Path) -> None:
    if sys.platform.startswith("win"):
        pytest.skip("chmod bits don't apply on Windows")
    cli._save_session(cli.Session(api_url="http://x", api_key="k", org="o"))
    mode = tmp_config.stat().st_mode & 0o777
    assert mode == 0o600, f"want 0o600 (owner read/write only), got {oct(mode)}"


def test_clear_session_removes_file(tmp_config: Path) -> None:
    cli._save_session(cli.Session(api_url="http://x", api_key="k", org="o"))
    assert tmp_config.exists()
    cli._clear_session()
    assert not tmp_config.exists()


def test_clear_session_is_noop_when_missing(tmp_config: Path) -> None:
    # Must not raise.
    cli._clear_session()


# -------- CLI commands ----------------------------------------------------


def test_status_without_session_returns_not_logged_in(tmp_config: Path,
                                                       capsys: pytest.CaptureFixture) -> None:
    with pytest.raises(SystemExit) as ex:
        cli.main(["status"])
    assert ex.value.code == cli.EXIT_NOT_LOGGED_IN
    err = capsys.readouterr().err
    assert "Not logged in" in err


def test_logout_removes_config(tmp_config: Path,
                                capsys: pytest.CaptureFixture) -> None:
    _write_session(tmp_config)
    rc = cli.main(["logout"])
    assert rc == cli.EXIT_OK
    assert not tmp_config.exists()
    assert "Removed" in capsys.readouterr().out


def test_whoami_prints_org_and_url(tmp_config: Path,
                                    capsys: pytest.CaptureFixture) -> None:
    _write_session(tmp_config, org="acme", api_url="http://api.example")
    rc = cli.main(["whoami"])
    assert rc == cli.EXIT_OK
    out = capsys.readouterr().out
    assert "acme" in out
    assert "http://api.example" in out


def test_login_with_bad_key_returns_auth_failed(tmp_config: Path,
                                                 capsys: pytest.CaptureFixture) -> None:
    """A 401 from the verification call must NOT save the config."""
    fake_client = _FakeClient(should_raise=RuntimeError("api error 401 unauthorized"))
    with patch.object(cli, "Client", return_value=fake_client):
        rc = cli.main(["login", "--api-url", "http://x", "--key", "bad-key"])
    assert rc == cli.EXIT_AUTH_FAILED
    assert not tmp_config.exists(), "bad key must NOT be persisted"


def test_login_with_good_key_saves_and_reports(tmp_config: Path,
                                                capsys: pytest.CaptureFixture) -> None:
    fake_client = _FakeClient(usage=_FakeUsage(fork_count=7, fork_total_ms=0))
    with patch.object(cli, "Client", return_value=fake_client):
        rc = cli.main(["login", "--api-url", "http://x", "--key", "acme:secret"])
    assert rc == cli.EXIT_OK
    session = cli._load_session()
    assert session is not None
    assert session.api_key == "acme:secret"
    assert session.org == "acme"
    out = capsys.readouterr().out
    assert "acme" in out
    assert "fork_count: 7" in out


def test_login_from_env_var_without_prompt(tmp_config: Path,
                                            monkeypatch: pytest.MonkeyPatch,
                                            capsys: pytest.CaptureFixture) -> None:
    monkeypatch.setenv("NANOVM_API_KEY", "acme:from-env")
    fake_client = _FakeClient(usage=_FakeUsage(fork_count=0, fork_total_ms=0))
    with patch.object(cli, "Client", return_value=fake_client):
        rc = cli.main(["login", "--api-url", "http://x"])
    assert rc == cli.EXIT_OK
    session = cli._load_session()
    assert session is not None
    assert session.api_key == "acme:from-env"


# -------- fakes -----------------------------------------------------------


class _FakeUsage:
    def __init__(self, fork_count: int, fork_total_ms: int) -> None:
        self.fork_count = fork_count
        self.fork_total_ms = fork_total_ms


class _FakeClient:
    """Stand-in for `nanovm.Client` in login/status tests.

    Only the entry points the CLI actually calls are stubbed. Anything
    else raises so a future CLI method that reaches into the client
    without a matching stub fails loudly rather than pretending to
    work.
    """

    def __init__(self, *,
                 usage: _FakeUsage | None = None,
                 should_raise: Exception | None = None) -> None:
        self._usage = usage
        self._raise = should_raise

    def usage(self) -> _FakeUsage:
        if self._raise:
            raise self._raise
        assert self._usage is not None
        return self._usage
