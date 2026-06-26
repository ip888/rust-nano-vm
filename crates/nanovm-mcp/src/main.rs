//! `nanovm-mcp` — Model Context Protocol stdio bridge for
//! rust-nano-vm.
//!
//! Speaks MCP (JSON-RPC 2.0 over stdio, newline-framed) on the host
//! side and translates `tools/call` invocations into HTTP calls to
//! the control-plane's `POST /v1/sandbox/invoke` on the other side.
//!
//! Wired into Claude Desktop / Claude Code / Cursor via:
//!
//! ```jsonc
//! // ~/.config/claude/claude_desktop_config.json (Claude Desktop)
//! // .mcp.json (Claude Code, repo-local)
//! // .cursor/mcp.json (Cursor)
//! {
//!   "mcpServers": {
//!     "nanovm": {
//!       "command": "/usr/local/bin/nanovm-mcp",
//!       "env": {
//!         "NANOVM_BASE_URL": "http://localhost:8080",
//!         "NANOVM_API_TOKEN": "dev-token",
//!         "NANOVM_SANDBOX_SNAPSHOT_ID": "42"
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! After config, the host exposes `execute_python`, `execute_shell`,
//! `read_file`, `write_file`, `list_files` as native tools. Each
//! call goes through the standard fork → action → destroy lifecycle
//! on the control plane.

#![forbid(unsafe_code)]

use std::io::IsTerminal;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use nanovm_mcp::client::{Config, SandboxClient};
use nanovm_mcp::mcp::{
    InitializeResult, Request, Response, RpcError, ServerCapabilities, ServerInfo, ToolsCapability,
    PROTOCOL_VERSION, PROTOCOL_VERSION_FALLBACK,
};
use nanovm_mcp::tools;

const SERVER_NAME: &str = "nanovm-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // MCP servers must NOT write anything other than JSON-RPC to
    // stdout — the host reads stdout as the wire. Send all tracing
    // to stderr so a poorly behaved log line can't corrupt the
    // protocol stream.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if std::io::stdin().is_terminal() {
        eprintln!(
            "{SERVER_NAME} {SERVER_VERSION}: this is an MCP stdio server. \
             It speaks JSON-RPC 2.0 on stdin/stdout and isn't useful from \
             an interactive terminal. Wire it into your MCP host (Claude \
             Desktop / Cursor / Claude Code) — see the crate README."
        );
    }

    let cfg = Config::from_env()?;
    tracing::info!(base_url = %cfg.base_url, "nanovm-mcp starting");
    let client = SandboxClient::new(cfg)?;

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle_request(&client, req).await,
            Err(e) => {
                tracing::warn!(error = %e, line = %line, "drop malformed JSON-RPC frame");
                // Without an id we can't address a response back —
                // -32700 parse error is normally sent with id:null
                // per the JSON-RPC spec.
                Some(Response::err(
                    Value::Null,
                    RpcError {
                        code: -32700,
                        message: format!("parse error: {e}"),
                        data: None,
                    },
                ))
            }
        };
        if let Some(resp) = response {
            let bytes = serde_json::to_vec(&resp)?;
            stdout.write_all(&bytes).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

/// Returns `Some(resp)` for requests (anything with an `id`),
/// `None` for notifications (no `id`).
async fn handle_request(client: &SandboxClient, req: Request) -> Option<Response> {
    let is_notification = req.id.is_none();
    let id = req.id.clone().unwrap_or(Value::Null);

    let result = match req.method.as_str() {
        "initialize" => initialize(&req.params).map(|r| serde_json::to_value(r).unwrap()),
        "tools/list" => Ok(serde_json::json!({ "tools": tools::tool_list() })),
        "tools/call" => match tools_call(client, &req.params).await {
            Ok(v) => Ok(v),
            Err(e) => Err(e),
        },
        // Notifications and other no-reply methods.
        "notifications/initialized" | "notifications/cancelled" => return None,
        // `ping` is a useful liveness check some hosts send.
        "ping" => Ok(Value::Object(serde_json::Map::new())),
        other => Err(RpcError::method_not_found(other)),
    };

    if is_notification {
        return None;
    }
    Some(match result {
        Ok(value) => Response::ok(id, value),
        Err(e) => Response::err(id, e),
    })
}

fn initialize(params: &Value) -> Result<InitializeResult, RpcError> {
    // Echo back the protocol version the client asked for when we
    // support it; otherwise pick our latest. Hosts that send a
    // future version get our fallback so the handshake still
    // succeeds rather than the client giving up entirely.
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    let chosen = if requested == PROTOCOL_VERSION || requested == PROTOCOL_VERSION_FALLBACK {
        requested.to_owned()
    } else {
        PROTOCOL_VERSION.to_owned()
    };
    Ok(InitializeResult {
        protocol_version: chosen,
        capabilities: ServerCapabilities {
            tools: ToolsCapability {
                list_changed: Some(false),
            },
        },
        server_info: ServerInfo {
            name: SERVER_NAME,
            version: SERVER_VERSION,
        },
    })
}

async fn tools_call(client: &SandboxClient, params: &Value) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("missing `name`"))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));
    let result = tools::dispatch_tool_call(client, name, &arguments).await?;
    Ok(serde_json::to_value(result).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn initialize_echoes_known_protocol_version() {
        let r = initialize(&json!({ "protocolVersion": PROTOCOL_VERSION })).unwrap();
        assert_eq!(r.protocol_version, PROTOCOL_VERSION);
        assert_eq!(r.server_info.name, "nanovm-mcp");
    }

    #[test]
    fn initialize_falls_back_when_client_sends_future_version() {
        let r = initialize(&json!({ "protocolVersion": "9999-99-99" })).unwrap();
        assert_eq!(r.protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn initialize_advertises_tools_capability() {
        let r = initialize(&json!({})).unwrap();
        assert_eq!(r.capabilities.tools.list_changed, Some(false));
    }
}
