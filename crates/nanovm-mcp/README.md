# nanovm-mcp — MCP stdio bridge for rust-nano-vm

A single-binary [Model Context Protocol] server that exposes the
`rust-nano-vm` sandbox-action API as native tools to MCP hosts like
Claude Desktop, Claude Code, Cursor, Cline, Goose, etc. Each tool
call goes through the standard fork → run-action → destroy lifecycle
on the control plane, so the LLM gets a fresh microVM (~12 ms cold
start on real KVM) for every invocation and the host gets the result
without ever having to touch a VM lifecycle.

[Model Context Protocol]: https://modelcontextprotocol.io

## Tools exposed

| Tool name        | What it does in the sandbox                                    |
|------------------|----------------------------------------------------------------|
| `execute_python` | `python3 -c <code>`                                            |
| `execute_shell`  | `sh -c <command>`                                              |
| `read_file`      | read file → returned as the tool's text content                |
| `write_file`     | write file (`mode` defaults to 0o644)                          |
| `list_files`     | `ls -1 -- <path>`                                              |

Each tool corresponds 1:1 to a `SandboxAction` variant on the
control-plane's [`POST /v1/sandbox/invoke`](../control-plane/src/sandbox.rs)
endpoint. The MCP layer does no validation beyond JSON-Schema —
the server is the source of truth for what each action accepts and
returns a 422 with a structured envelope on any mismatch.

## Architecture

```
Claude Desktop / Claude Code / Cursor
    │   (MCP JSON-RPC 2.0, newline-framed, over stdio)
    ▼
┌─────────────────────────────────┐
│ nanovm-mcp  (this crate)        │
│   - initialize / tools/list     │
│   - tools/call → HTTP POST      │
└─────────────────────────────────┘
    │   (HTTP, optional bearer token)
    ▼
nanovm-control-plane :8080
    │
    ▼  POST /v1/sandbox/invoke
fork-from-snapshot → run action → destroy → return SandboxResult
```

The bridge process holds one `reqwest::Client` for connection reuse
so back-to-back tool calls don't re-handshake TLS. Logs go to
**stderr only** — stdout is the MCP wire, so a stray log line would
corrupt the protocol.

## Install

```sh
cargo install --path crates/nanovm-mcp
# Binary lands at $CARGO_HOME/bin/nanovm-mcp
```

A release binary is also published as
`ghcr.io/ip888/nanovm-mcp:<tag>` once tags ship.

## Configuration

Three environment variables, all read once at startup:

| Variable                       | Default                   | Purpose |
|--------------------------------|---------------------------|---------|
| `NANOVM_BASE_URL`              | `http://localhost:8080`   | Control plane root (trailing slashes stripped). |
| `NANOVM_API_TOKEN`             | unset (auth-disabled mode)| Bearer token, sent on every request when set. |
| `NANOVM_SANDBOX_SNAPSHOT_ID`   | unset                     | Default snapshot id forwarded when the LLM doesn't pass `snapshot` itself. Falls back to the server's own default if unset on both. |

A malformed `NANOVM_SANDBOX_SNAPSHOT_ID` (anything that doesn't parse
as a `u64`) fails the binary at startup with an actionable error —
mirrors the server's policy and avoids the security footgun where a
typo silently drops the snapshot pin.

## Wire-up

### Claude Code

Repo-local `.mcp.json`:

```json
{
  "mcpServers": {
    "nanovm": {
      "command": "nanovm-mcp",
      "env": {
        "NANOVM_BASE_URL": "http://localhost:8080",
        "NANOVM_API_TOKEN": "dev-token",
        "NANOVM_SANDBOX_SNAPSHOT_ID": "42"
      }
    }
  }
}
```

### Claude Desktop

`~/Library/Application Support/Claude/claude_desktop_config.json`
(macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "nanovm": {
      "command": "/usr/local/bin/nanovm-mcp",
      "env": { /* same as above */ }
    }
  }
}
```

### Cursor

`.cursor/mcp.json` in the workspace root, same shape.

## What the LLM sees

Every tool call comes back as one MCP `text` content fragment shaped
like:

```
[exit=0 duration_ms=14 cold_start=cold]
--- stdout ---
hello sandbox
```

Non-zero exit codes flip the MCP `isError` flag on the result so the
host UI can render the call as failed. Control-plane HTTP errors
fold into the same shape (with `isError = true`) so the LLM sees the
failure as a tool result rather than the host treating the call as
broken. Transport errors (control plane unreachable) surface as
JSON-RPC errors so the host can offer to retry.

## What this is NOT

- **Not a stand-alone sandbox.** The bridge assumes a running
  `nanovm-control-plane` somewhere. Without it, every tool call
  returns a transport error.
- **Not a translation layer.** Tool names match the server's
  `SandboxAction` discriminator verbatim. Adding a new action
  server-side requires a small change here (one entry in
  `tool_list()`) but no contract changes.
- **Not multi-session.** One `nanovm-mcp` process serves one MCP
  client. Hosts that need to fan multiple agents at one bridge
  should spawn multiple processes.
