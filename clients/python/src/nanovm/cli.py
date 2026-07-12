"""``nanovm`` command-line entry point.

Ships with the SDK — ``pip install nanovm`` gets you both the Python
Client API and the ``nanovm`` shell command. Same auth, same API host,
same tenant.

Config lives in the platform-appropriate config directory:

- **Linux**: ``$XDG_CONFIG_HOME/nanovm/config.json`` (default
  ``~/.config/nanovm/config.json``)
- **macOS / Windows / no XDG**: ``~/.nanovm/config.json``

The file holds only ``{"api_url": ..., "api_key": ..., "org": ...}``.
Chmod 0600 on Unix. Deleting the file logs the user out — no
server-side session to invalidate.

Commands are deliberately minimal and match the "give it a try in 30
seconds" story:

    nanovm login                      # paste key, verify, save
    nanovm status                     # plan + usage tile
    nanovm whoami                     # same as status but terse
    nanovm python 'print(1+1)'        # quick sandbox exec
    nanovm shell 'echo hi'            # ditto for shell
    nanovm logout                     # forget the key
"""

from __future__ import annotations

import argparse
import json
import os
import stat
import sys
from dataclasses import dataclass
from getpass import getpass
from pathlib import Path
from typing import Optional

from . import AuthError, Client, NanovmError, __version__

# Exit codes — grep-able for shell scripts calling this CLI.
EXIT_OK = 0
EXIT_USAGE = 2
EXIT_NOT_LOGGED_IN = 3
EXIT_API_ERROR = 4
EXIT_AUTH_FAILED = 5


# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------


@dataclass
class Session:
    """A logged-in session, mirroring what the web dashboard stores."""

    api_url: str
    api_key: str
    org: str


def _config_path() -> Path:
    """Where the CLI persists its session.

    Uses XDG_CONFIG_HOME on Linux, ~/.nanovm/config.json elsewhere
    (macOS, Windows). Env var ``NANOVM_CONFIG`` overrides for tests
    and for users who want to keep multiple identities.
    """
    if override := os.environ.get("NANOVM_CONFIG"):
        return Path(override)
    if sys.platform.startswith("linux"):
        xdg = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
        return Path(xdg) / "nanovm" / "config.json"
    return Path.home() / ".nanovm" / "config.json"


def _load_session() -> Optional[Session]:
    path = _config_path()
    if not path.exists():
        return None
    try:
        raw = json.loads(path.read_text())
    except (OSError, ValueError):
        return None
    keys = {"api_url", "api_key", "org"}
    if not keys.issubset(raw):
        return None
    return Session(api_url=raw["api_url"], api_key=raw["api_key"], org=raw["org"])


def _save_session(session: Session) -> None:
    path = _config_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(
            {
                "api_url": session.api_url,
                "api_key": session.api_key,
                "org": session.org,
            }
        )
    )
    # Best-effort chmod 0600 on Unix. Windows silently ignores the
    # bits — that's fine, the file lives under the user's profile.
    try:
        path.chmod(stat.S_IRUSR | stat.S_IWUSR)
    except OSError:
        pass


def _clear_session() -> None:
    path = _config_path()
    try:
        path.unlink()
    except FileNotFoundError:
        pass


def _client_from(session: Session) -> Client:
    return Client(base_url=session.api_url, token=session.api_key)


def _require_session() -> Session:
    session = _load_session()
    if session is None:
        print(
            "Not logged in. Run `nanovm login` first, or set NANOVM_CONFIG to a\n"
            "config file you already have.",
            file=sys.stderr,
        )
        sys.exit(EXIT_NOT_LOGGED_IN)
    return session


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------


def cmd_login(args: argparse.Namespace) -> int:
    """`nanovm login` — prompt for API key + save."""
    api_url = args.api_url
    api_key = args.key or os.environ.get("NANOVM_API_KEY")
    if not api_key:
        try:
            api_key = getpass("API key (input hidden): ").strip()
        except (EOFError, KeyboardInterrupt):
            print("\nAborted.", file=sys.stderr)
            return EXIT_USAGE
    if not api_key:
        print("No API key entered.", file=sys.stderr)
        return EXIT_USAGE

    # Verify BEFORE saving so a bad key doesn't leave the user with a
    # broken config they have to hand-edit.
    client = Client(base_url=api_url, token=api_key)
    try:
        # /v1/usage is auth'd and cheap; a 401 tells us the key's dead.
        usage = client.usage()
    except AuthError as e:
        print(f"API key rejected: {e}", file=sys.stderr)
        return EXIT_AUTH_FAILED
    except NanovmError as e:
        if e.status == 401:
            print(f"API key rejected: {e}", file=sys.stderr)
            return EXIT_AUTH_FAILED
        print(f"Couldn't reach the API at {api_url}: {e}", file=sys.stderr)
        return EXIT_API_ERROR
    except Exception as e:  # noqa: BLE001 — bubbled up as CLI text
        print(f"Couldn't reach the API at {api_url}: {e}", file=sys.stderr)
        return EXIT_API_ERROR

    # Derive org from the token shape (`org:secret`); fall back to
    # `?` for tokens the CLI doesn't recognise.
    org = api_key.split(":", 1)[0] if ":" in api_key else "?"
    try:
        _save_session(Session(api_url=api_url, api_key=api_key, org=org))
    except OSError as e:
        print(f"Couldn't save session to {_config_path()}: {e}", file=sys.stderr)
        return EXIT_API_ERROR
    print(f"Logged in as {org} at {api_url}.")
    print(f"  fork_count: {usage.fork_count}")
    print(f"  saved to:   {_config_path()}")
    return EXIT_OK


def cmd_logout(_args: argparse.Namespace) -> int:
    """`nanovm logout` — delete config."""
    try:
        _clear_session()
    except OSError as e:
        print(f"Couldn't remove {_config_path()}: {e}", file=sys.stderr)
        return EXIT_API_ERROR
    print(f"Removed {_config_path()}.")
    return EXIT_OK


def cmd_whoami(_args: argparse.Namespace) -> int:
    """`nanovm whoami` — one-line identity."""
    session = _require_session()
    print(f"{session.org}\t{session.api_url}")
    return EXIT_OK


def cmd_status(_args: argparse.Namespace) -> int:
    """`nanovm status` — plan + usage in a two-column layout."""
    session = _require_session()
    client = _client_from(session)
    try:
        usage = client.usage()
    except Exception as e:  # noqa: BLE001
        print(f"API error: {e}", file=sys.stderr)
        return EXIT_API_ERROR

    lines = [
        ("Org", session.org),
        ("API", session.api_url),
        ("Forks", str(usage.fork_count)),
        ("Total ms", str(usage.fork_total_ms)),
    ]
    avg_ms = usage.fork_total_ms // usage.fork_count if usage.fork_count else 0
    lines.append(("Avg ms/fork", str(avg_ms)))

    width = max(len(k) for k, _ in lines)
    for k, v in lines:
        print(f"{k:<{width}}  {v}")
    return EXIT_OK


def cmd_python(args: argparse.Namespace) -> int:
    """`nanovm python 'print(1+1)'` — one-shot Python in a sandbox."""
    session = _require_session()
    client = _client_from(session)
    try:
        result = client.execute_python(
            args.code,
            snapshot=args.snapshot,
            timeout_ms=args.timeout,
        )
    except Exception as e:  # noqa: BLE001
        print(f"API error: {e}", file=sys.stderr)
        return EXIT_API_ERROR
    if result.stdout:
        sys.stdout.write(result.stdout)
        if not result.stdout.endswith("\n"):
            sys.stdout.write("\n")
    if result.stderr:
        sys.stderr.write(result.stderr)
    return result.exit_code


def cmd_shell(args: argparse.Namespace) -> int:
    """`nanovm shell 'echo hi'` — one-shot shell in a sandbox."""
    session = _require_session()
    client = _client_from(session)
    try:
        result = client.execute_shell(
            args.command,
            snapshot=args.snapshot,
            timeout_ms=args.timeout,
        )
    except Exception as e:  # noqa: BLE001
        print(f"API error: {e}", file=sys.stderr)
        return EXIT_API_ERROR
    if result.stdout:
        sys.stdout.write(result.stdout)
        if not result.stdout.endswith("\n"):
            sys.stdout.write("\n")
    if result.stderr:
        sys.stderr.write(result.stderr)
    return result.exit_code


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def _build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="nanovm",
        description=(
            "Command-line client for rust-nano-vm. Same auth + API as the "
            "Python SDK — pass --key on `login` or set NANOVM_API_KEY."
        ),
    )
    p.add_argument("--version", action="version", version=f"nanovm {__version__}")
    sub = p.add_subparsers(dest="command", required=True, metavar="COMMAND")

    login = sub.add_parser("login", help="Save API key + verify it.")
    login.add_argument(
        "--api-url",
        default=os.environ.get("NANOVM_API_URL", "http://localhost:8080"),
        help="Control-plane base URL. Default env NANOVM_API_URL or "
        "http://localhost:8080.",
    )
    login.add_argument(
        "--key",
        default=None,
        help="API key. Omit to be prompted (or set NANOVM_API_KEY).",
    )
    login.set_defaults(func=cmd_login)

    logout = sub.add_parser("logout", help="Forget the saved API key.")
    logout.set_defaults(func=cmd_logout)

    whoami = sub.add_parser("whoami", help="Show the org + API URL.")
    whoami.set_defaults(func=cmd_whoami)

    status = sub.add_parser("status", help="Show current usage.")
    status.set_defaults(func=cmd_status)

    py = sub.add_parser("python", help="Run a Python snippet in a sandbox.")
    py.add_argument("code", help="Python source to execute.")
    py.add_argument(
        "--snapshot",
        type=int,
        default=None,
        help="Snapshot id to fork. Default = server-side default.",
    )
    py.add_argument(
        "--timeout",
        type=int,
        default=None,
        help="Timeout in milliseconds. Default = server-side default.",
    )
    py.set_defaults(func=cmd_python)

    sh = sub.add_parser("shell", help="Run a shell command in a sandbox.")
    sh.add_argument("command", help="Shell command to execute.")
    sh.add_argument("--snapshot", type=int, default=None)
    sh.add_argument("--timeout", type=int, default=None)
    sh.set_defaults(func=cmd_shell)

    return p


def main(argv: Optional[list[str]] = None) -> int:
    args = _build_parser().parse_args(argv)
    return args.func(args)


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
