//! Minimal MCP (Model Context Protocol) JSON-RPC 2.0 types.
//!
//! We don't pull in the full `rmcp` crate because we only implement
//! the three request handlers an AI-agent code-execution bridge
//! actually needs — `initialize`, `tools/list`, `tools/call` — plus
//! a handful of notifications. Hand-rolling keeps the dep surface
//! tiny and the wire shape easy to audit.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC version. The MCP spec pins 2.0.
pub const JSONRPC_VERSION: &str = "2.0";

/// MCP protocol revisions we support. The spec is versioned by date
/// string; the client sends one in `initialize`, we echo back the
/// same value if we support it, else echo our latest.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const PROTOCOL_VERSION_FALLBACK: &str = "2024-11-05";

/// Incoming JSON-RPC envelope. `id` is `None` for notifications
/// (requests that expect no reply); when present it must round-trip
/// back on the response so the client correlates.
#[derive(Debug, Deserialize)]
pub struct Request {
    /// Required by the spec ("2.0"); we accept and discard. Marked
    /// `dead_code`-allowed because serde consumes it on parse and
    /// the rest of the handler doesn't need to look at it.
    #[allow(dead_code)]
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Outgoing JSON-RPC response — `result` xor `error`, never both.
#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// JSON-RPC error envelope. Codes follow the JSON-RPC spec:
/// -32700 parse error, -32600 invalid request, -32601 method not
/// found, -32602 invalid params, -32603 internal error. Anything
/// in the -32000…-32099 range is server-defined.
#[derive(Debug, Serialize, thiserror::Error)]
#[error("rpc error {code}: {message}")]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {method}"),
            data: None,
        }
    }

    pub fn invalid_params(detail: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: format!("invalid params: {}", detail.into()),
            data: None,
        }
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: format!("internal error: {}", detail.into()),
            data: None,
        }
    }
}

// --- MCP-specific payloads -------------------------------------------------

/// Server identity reported on the `initialize` handshake. Names
/// here surface in the host UI (Claude Desktop, Cursor, Claude
/// Code, …) so they're worth getting right.
#[derive(Debug, Serialize)]
pub struct ServerInfo {
    pub name: &'static str,
    pub version: &'static str,
}

/// The capability advertisement in `initialize`. We only run tools,
/// so we set `tools: {}` and leave the others off — clients that
/// support more (resources, prompts, sampling) just won't ask.
#[derive(Debug, Serialize)]
pub struct ServerCapabilities {
    pub tools: ToolsCapability,
}

/// Empty struct is the canonical "tools supported, nothing fancier"
/// signal. Some clients gate behind the field being present at all.
#[derive(Debug, Serialize)]
pub struct ToolsCapability {
    #[serde(rename = "listChanged", skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
}

/// A single tool definition for `tools/list`. Input schema is a JSON
/// Schema object describing the `arguments` shape the client should
/// send to `tools/call`.
#[derive(Debug, Serialize)]
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Successful tool result. `is_error: false` flips to `true` when
/// the tool ran but the underlying op (a program exit, a 404 from
/// the control plane) wasn't successful — distinct from a transport
/// failure, which surfaces as a JSON-RPC error response instead.
#[derive(Debug, Serialize)]
pub struct ToolResult {
    pub content: Vec<ContentPart>,
    #[serde(rename = "isError", skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

/// One content fragment in a tool result. We only emit `text`; the
/// MCP spec also allows `image`, `resource`, `audio`, etc. but the
/// sandbox-action API doesn't produce any of those.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ContentPart {
    Text { text: String },
}

impl ContentPart {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_ok_serializes_without_error_field() {
        let r = Response::ok(Value::from(1), serde_json::json!({"x": 1}));
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""jsonrpc":"2.0""#));
        assert!(s.contains(r#""id":1"#));
        assert!(s.contains(r#""result":{"x":1}"#));
        assert!(!s.contains(r#""error""#));
    }

    #[test]
    fn response_err_serializes_without_result_field() {
        let r = Response::err(Value::from(7), RpcError::method_not_found("foo/bar"));
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""error""#));
        assert!(s.contains(r#""code":-32601"#));
        assert!(s.contains("foo/bar"));
        assert!(!s.contains(r#""result""#));
    }

    #[test]
    fn rpc_error_codes_match_jsonrpc_spec() {
        assert_eq!(RpcError::method_not_found("x").code, -32601);
        assert_eq!(RpcError::invalid_params("x").code, -32602);
        assert_eq!(RpcError::internal("x").code, -32603);
    }

    #[test]
    fn tool_result_omits_is_error_when_false() {
        let r = ToolResult {
            content: vec![ContentPart::text("hi")],
            is_error: false,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("isError"));
    }

    #[test]
    fn content_part_text_serializes_with_tagged_type() {
        let s = serde_json::to_string(&ContentPart::text("hello")).unwrap();
        assert_eq!(s, r#"{"type":"text","text":"hello"}"#);
    }
}
