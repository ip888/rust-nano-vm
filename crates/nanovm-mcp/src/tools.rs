//! Tool definitions for the MCP stdio bridge.
//!
//! Each tool corresponds 1:1 to an `action` value on the control
//! plane's `POST /v1/sandbox/invoke`. The MCP layer hands a JSON
//! Schema to the LLM so it knows what arguments to send; we then
//! flatten those arguments into the action discriminator + per-action
//! fields the server expects.

use serde_json::{json, Value};

use crate::client::{SandboxClient, SandboxResult};
use crate::mcp::{ContentPart, RpcError, Tool, ToolResult};

/// Static tool definitions surfaced on `tools/list`. Names match the
/// server-side `SandboxAction` discriminator verbatim so the
/// dispatch in [`dispatch_tool_call`] is a simple `match`.
pub fn tool_list() -> Vec<Tool> {
    vec![
        Tool {
            name: "execute_python",
            description: "Run a Python 3 program in an ephemeral microVM sandbox and return stdout, stderr, and exit code. Each call gets a fresh VM (~12 ms cold start on real KVM); the VM is destroyed after the program exits. Use this for any code-execution tool-use where Python is the right language.",
            input_schema: json!({
                "type": "object",
                "required": ["code"],
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Python program body. Passed verbatim to `python3 -c`."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Wall-clock timeout in milliseconds. The server enforces its own default when omitted."
                    },
                    "snapshot": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Snapshot id to fork from. Omit to use the server's default (NANOVM_SANDBOX_SNAPSHOT_ID)."
                    }
                }
            }),
        },
        Tool {
            name: "execute_shell",
            description: "Run a shell command (`sh -c <command>`) in an ephemeral microVM sandbox. Each call gets a fresh VM; the VM is destroyed after the command exits. Use for shell pipelines, system inspection, or running compiled programs.",
            input_schema: json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command. Passed verbatim to `sh -c`."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 0
                    },
                    "snapshot": {
                        "type": "integer",
                        "minimum": 0
                    }
                }
            }),
        },
        Tool {
            name: "read_file",
            description: "Read a file from the guest filesystem of an ephemeral sandbox VM. File content is returned (UTF-8 lossy) in the tool's text output. The path must be absolute inside the guest.",
            input_schema: json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string" },
                    "snapshot": { "type": "integer", "minimum": 0 }
                }
            }),
        },
        Tool {
            name: "write_file",
            description: "Write a file to the guest filesystem of an ephemeral sandbox VM. The VM is destroyed after the write completes; combine with execute_shell/execute_python in the same agent loop if you want to operate on the file. `mode` defaults to 0o644.",
            input_schema: json!({
                "type": "object",
                "required": ["path", "content"],
                "properties": {
                    "path":    { "type": "string" },
                    "content": { "type": "string" },
                    "mode":    { "type": "integer", "minimum": 0 },
                    "snapshot":{ "type": "integer", "minimum": 0 }
                }
            }),
        },
        Tool {
            name: "list_files",
            description: "List directory entries (`ls -1 -- <path>`) in an ephemeral sandbox VM. One entry per line in the tool's text output.",
            input_schema: json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string" },
                    "snapshot": { "type": "integer", "minimum": 0 }
                }
            }),
        },
    ]
}

/// Build the JSON body for `POST /v1/sandbox/invoke` from an MCP
/// `tools/call` invocation. Mirrors the tagged-union shape the
/// server expects: `{action: "<name>", ...<args>}`.
pub fn build_invoke_body(tool_name: &str, arguments: &Value) -> Result<Value, RpcError> {
    if !is_known_tool(tool_name) {
        return Err(RpcError::method_not_found(tool_name));
    }
    let mut body = match arguments {
        Value::Object(o) => Value::Object(o.clone()),
        Value::Null => Value::Object(serde_json::Map::new()),
        _ => return Err(RpcError::invalid_params("`arguments` must be an object")),
    };
    if let Some(obj) = body.as_object_mut() {
        // Push `action` to the front for readability in any audit
        // log; not semantically required, but cheap.
        obj.insert("action".to_owned(), Value::from(tool_name));
    }
    Ok(body)
}

fn is_known_tool(name: &str) -> bool {
    matches!(
        name,
        "execute_python" | "execute_shell" | "read_file" | "write_file" | "list_files"
    )
}

/// Run a tool against the control plane and render the result as an
/// MCP `ToolResult`. Transport / parse failures bubble as `RpcError`
/// so the JSON-RPC layer can return a proper error response; HTTP
/// non-2xx responses fold into a `ToolResult { is_error: true }` so
/// the LLM sees the failure without the host treating it as a
/// transport problem.
pub async fn dispatch_tool_call(
    client: &SandboxClient,
    tool_name: &str,
    arguments: &Value,
) -> Result<ToolResult, RpcError> {
    let body = build_invoke_body(tool_name, arguments)?;
    match client.invoke(body).await {
        Ok(result) => Ok(render_success(tool_name, &result)),
        Err(crate::client::InvokeError::Http { status, body }) => Ok(ToolResult {
            content: vec![ContentPart::text(format!(
                "sandbox returned HTTP {status}: {body}"
            ))],
            is_error: true,
        }),
        Err(crate::client::InvokeError::Transport(msg)) => Err(RpcError::internal(format!(
            "control-plane unreachable: {msg}"
        ))),
        Err(crate::client::InvokeError::BadResponse(msg)) => Err(RpcError::internal(msg)),
    }
}

/// Render a successful invoke into the MCP content shape. For
/// execute_* actions we surface the captured stdout + stderr +
/// exit code in a way an LLM can read. For file-op actions stdout
/// already carries the payload (file content / "bytes_written=N" /
/// ls output) so we don't double-print.
fn render_success(tool_name: &str, r: &SandboxResult) -> ToolResult {
    let cold = if r.cold_start { "cold" } else { "warm" };
    let header = format!(
        "[exit={} duration_ms={} cold_start={cold}]",
        r.exit_code, r.duration_ms
    );
    let body = match tool_name {
        // Files: stdout is the whole payload. Skip stderr unless
        // there is something there.
        "read_file" | "list_files" | "write_file" => {
            if r.stderr.is_empty() {
                r.stdout.clone()
            } else {
                format!("{}\n--- stderr ---\n{}", r.stdout, r.stderr)
            }
        }
        // Exec: render both streams labeled, since the LLM may need
        // to read them separately when diagnosing a failure.
        _ => {
            let mut out = String::new();
            if !r.stdout.is_empty() {
                out.push_str("--- stdout ---\n");
                out.push_str(&r.stdout);
                if !r.stdout.ends_with('\n') {
                    out.push('\n');
                }
            }
            if !r.stderr.is_empty() {
                out.push_str("--- stderr ---\n");
                out.push_str(&r.stderr);
                if !r.stderr.ends_with('\n') {
                    out.push('\n');
                }
            }
            out
        }
    };
    let is_error = r.exit_code != 0;
    let mut text = header;
    if !body.is_empty() {
        text.push('\n');
        text.push_str(&body);
    }
    ToolResult {
        content: vec![ContentPart::text(text)],
        is_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_advertises_five_tools() {
        let tools = tool_list();
        assert_eq!(tools.len(), 5);
        let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "execute_python",
                "execute_shell",
                "read_file",
                "write_file",
                "list_files",
            ]
        );
    }

    #[test]
    fn build_invoke_body_injects_action_discriminator() {
        let body = build_invoke_body("execute_python", &json!({"code": "print(1)"})).unwrap();
        assert_eq!(body["action"], "execute_python");
        assert_eq!(body["code"], "print(1)");
    }

    #[test]
    fn build_invoke_body_accepts_null_arguments() {
        let body = build_invoke_body("list_files", &Value::Null).unwrap();
        assert_eq!(body["action"], "list_files");
    }

    #[test]
    fn build_invoke_body_rejects_unknown_tool_with_method_not_found() {
        let err = build_invoke_body("rm_rf", &json!({})).unwrap_err();
        assert_eq!(err.code, -32601);
    }

    #[test]
    fn build_invoke_body_rejects_non_object_arguments() {
        let err = build_invoke_body("execute_shell", &json!("not an object")).unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn render_success_marks_non_zero_exit_as_error() {
        let r = SandboxResult {
            stdout: "".to_owned(),
            stderr: "boom".to_owned(),
            exit_code: 1,
            duration_ms: 10,
            cold_start: true,
        };
        let out = render_success("execute_shell", &r);
        assert!(out.is_error);
        let text = match &out.content[0] {
            ContentPart::Text { text } => text.clone(),
        };
        assert!(text.contains("exit=1"));
        assert!(text.contains("boom"));
    }

    #[test]
    fn render_success_for_read_file_omits_stream_labels() {
        let r = SandboxResult {
            stdout: "file contents\n".to_owned(),
            stderr: "".to_owned(),
            exit_code: 0,
            duration_ms: 5,
            cold_start: false,
        };
        let out = render_success("read_file", &r);
        assert!(!out.is_error);
        let text = match &out.content[0] {
            ContentPart::Text { text } => text.clone(),
        };
        assert!(text.contains("file contents"));
        assert!(text.contains("cold_start=warm"));
        assert!(!text.contains("--- stdout ---"));
    }
}
