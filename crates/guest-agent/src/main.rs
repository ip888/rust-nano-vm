//! `nanovm-agent` — the in-guest sandbox agent binary.
//!
//! # Scope (M2)
//!
//! Runs inside every sandbox VM and speaks the `agent-sandbox-proto` protocol
//! with the host VMM over `virtio-vsock`. The binary is compiled as a static
//! `x86_64-unknown-linux-musl` binary so it can run inside a minimal
//! initramfs without a full libc.
//!
//! ## M2 transport wiring (not yet landed)
//!
//! In M2 the agent will:
//!
//! 1. Open `/dev/vsock` and bind to the well-known agent port.
//! 2. Accept a connection from the host (CID 2).
//! 3. Enter a request loop: read a length-prefixed JSON
//!    [`proto::Request`], dispatch to the handler, write back a
//!    [`proto::Response`].
//!
//! ## Stdin / stdout mode (current, for local testing without KVM)
//!
//! Until vsock is wired, the agent reads newline-delimited JSON requests
//! from `stdin` and writes newline-delimited JSON responses to `stdout`.
//! This makes it trivially testable with:
//!
//! ```sh
//! echo '{"version":1,"id":{"0":1},"body":{"op":"ping"}}' | cargo run -p guest-agent
//! ```

#![forbid(unsafe_code)]

use std::io::{self, BufRead, Write};

use proto::{ErrorCode, Request, Response, ResponseBody, RpcError, PROTOCOL_VERSION};

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) if l.trim().is_empty() => continue,
            Ok(l) => l,
            Err(e) => {
                eprintln!("nanovm-agent: stdin read error: {e}");
                break;
            }
        };

        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response {
                    version: PROTOCOL_VERSION,
                    id: proto::RequestId(0),
                    result: Err(RpcError {
                        code: ErrorCode::BadRequest,
                        message: format!("malformed request: {e}"),
                    }),
                };
                let _ = write_response(&mut out, &resp);
                continue;
            }
        };

        if req.version != PROTOCOL_VERSION {
            let resp = Response {
                version: PROTOCOL_VERSION,
                id: req.id,
                result: Err(RpcError {
                    code: ErrorCode::VersionMismatch,
                    message: format!(
                        "expected protocol version {PROTOCOL_VERSION}, got {}",
                        req.version
                    ),
                }),
            };
            let _ = write_response(&mut out, &resp);
            continue;
        }

        let result = dispatch(req.body);
        let resp = Response {
            version: PROTOCOL_VERSION,
            id: req.id,
            result,
        };
        if write_response(&mut out, &resp).is_err() {
            break;
        }
    }
}

/// Route a parsed request body to the appropriate handler.
fn dispatch(body: proto::RequestBody) -> Result<ResponseBody, RpcError> {
    use proto::RequestBody;
    match body {
        RequestBody::Ping => Ok(ResponseBody::Pong),
        RequestBody::Exec {
            program,
            args,
            cwd,
            env,
            timeout_ms,
        } => handle_exec(program, args, cwd, env, timeout_ms),
        RequestBody::WriteFile {
            path,
            content,
            mode,
        } => handle_write_file(path, content, mode),
        RequestBody::ReadFile { path } => handle_read_file(path),
        RequestBody::Stat { path } => handle_stat(path),
        _ => Err(RpcError {
            code: ErrorCode::BadRequest,
            message: "operation requires M2 streaming support (not yet implemented)".into(),
        }),
    }
}

fn handle_exec(
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: Vec<(String, String)>,
    timeout_ms: Option<u64>,
) -> Result<ResponseBody, RpcError> {
    use std::process::Command;
    use std::time::Instant;

    let start = Instant::now();

    let mut cmd = Command::new(&program);
    cmd.args(&args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }

    // For M2 this is a simple blocking exec; streaming via ExecStart arrives
    // in the full virtio-vsock wiring.
    let output = cmd.output().map_err(|e| RpcError {
        code: ErrorCode::Io,
        message: format!("failed to spawn {program}: {e}"),
    })?;

    let duration_ms = start.elapsed().as_millis() as u64;

    if let Some(limit) = timeout_ms {
        if duration_ms > limit {
            return Err(RpcError {
                code: ErrorCode::Timeout,
                message: format!("process exceeded {limit}ms"),
            });
        }
    }

    Ok(ResponseBody::ExecResult {
        exit_code: output.status.code(),
        signal: None,
        stdout: output.stdout,
        stderr: output.stderr,
        duration_ms,
    })
}

fn handle_write_file(path: String, content: Vec<u8>, mode: u32) -> Result<ResponseBody, RpcError> {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let bytes = content.len() as u64;
    fs::write(&path, &content).map_err(|e| RpcError {
        code: ErrorCode::Io,
        message: format!("write {path}: {e}"),
    })?;

    #[cfg(unix)]
    {
        let perms = fs::Permissions::from_mode(mode);
        fs::set_permissions(&path, perms).map_err(|e| RpcError {
            code: ErrorCode::Io,
            message: format!("chmod {path}: {e}"),
        })?;
    }
    #[cfg(not(unix))]
    let _ = mode;

    Ok(ResponseBody::Written { bytes })
}

fn handle_read_file(path: String) -> Result<ResponseBody, RpcError> {
    let content = std::fs::read(&path).map_err(|e| RpcError {
        code: if e.kind() == std::io::ErrorKind::NotFound {
            ErrorCode::NotFound
        } else {
            ErrorCode::Io
        },
        message: format!("read {path}: {e}"),
    })?;
    Ok(ResponseBody::FileContent { content })
}

fn handle_stat(path: String) -> Result<ResponseBody, RpcError> {
    let meta = std::fs::metadata(&path).map_err(|e| RpcError {
        code: if e.kind() == std::io::ErrorKind::NotFound {
            ErrorCode::NotFound
        } else {
            ErrorCode::Io
        },
        message: format!("stat {path}: {e}"),
    })?;

    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::MetadataExt;
        meta.mode()
    };
    #[cfg(not(unix))]
    let mode: u32 = if meta.permissions().readonly() {
        0o444
    } else {
        0o644
    };

    Ok(ResponseBody::StatResult {
        size: meta.len(),
        mode,
        is_dir: meta.is_dir(),
    })
}

fn write_response(out: &mut impl Write, resp: &Response) -> io::Result<()> {
    let json = serde_json::to_string(resp).map_err(|e| io::Error::other(e.to_string()))?;
    writeln!(out, "{json}")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::RequestBody;

    #[test]
    fn ping_returns_pong() {
        let result = dispatch(RequestBody::Ping);
        assert!(matches!(result, Ok(ResponseBody::Pong)));
    }

    #[test]
    fn exec_echo_returns_exit_zero_and_captures_stdout() {
        let result = dispatch(RequestBody::Exec {
            program: "echo".into(),
            args: vec!["hello".into()],
            cwd: None,
            env: vec![],
            timeout_ms: None,
        });
        match result {
            Ok(ResponseBody::ExecResult {
                exit_code, stdout, ..
            }) => {
                assert_eq!(exit_code, Some(0));
                assert!(stdout.starts_with(b"hello"));
            }
            other => panic!("expected ExecResult, got {other:?}"),
        }
    }

    #[test]
    fn exec_missing_binary_returns_io_error() {
        let result = dispatch(RequestBody::Exec {
            program: "/no/such/binary".into(),
            args: vec![],
            cwd: None,
            env: vec![],
            timeout_ms: None,
        });
        assert!(matches!(
            result,
            Err(RpcError {
                code: ErrorCode::Io,
                ..
            })
        ));
    }

    #[test]
    fn write_then_read_roundtrips() {
        let path = format!("/tmp/nanovm-agent-test-{}", std::process::id());
        let content = b"hello from agent\n".to_vec();

        dispatch(RequestBody::WriteFile {
            path: path.clone(),
            content: content.clone(),
            mode: 0o644,
        })
        .unwrap();

        let result = dispatch(RequestBody::ReadFile { path: path.clone() });
        match result {
            Ok(ResponseBody::FileContent { content: got }) => assert_eq!(got, content),
            other => panic!("expected FileContent, got {other:?}"),
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stat_missing_file_returns_not_found() {
        let result = dispatch(RequestBody::Stat {
            path: "/no/such/file/for/agent/test".into(),
        });
        assert!(matches!(
            result,
            Err(RpcError {
                code: ErrorCode::NotFound,
                ..
            })
        ));
    }

    #[test]
    fn version_mismatch_surfaces_error_code() {
        let req = Request {
            version: 9999,
            id: proto::RequestId(1),
            body: RequestBody::Ping,
        };
        let mut buf = Vec::new();
        let json = serde_json::to_string(&req).unwrap();
        buf.extend_from_slice(json.as_bytes());
        buf.push(b'\n');

        let mut out = Vec::new();
        let stdin = io::Cursor::new(buf);
        for line in io::BufRead::lines(stdin) {
            let line = line.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            let result = if req.version != PROTOCOL_VERSION {
                Err(RpcError {
                    code: ErrorCode::VersionMismatch,
                    message: "version mismatch".to_string(),
                })
            } else {
                dispatch(req.body)
            };
            let resp = Response {
                version: PROTOCOL_VERSION,
                id: req.id,
                result,
            };
            writeln!(out, "{}", serde_json::to_string(&resp).unwrap()).unwrap();
        }
        let resp: Response = serde_json::from_slice(&out).unwrap();
        assert!(matches!(
            resp.result,
            Err(RpcError {
                code: ErrorCode::VersionMismatch,
                ..
            })
        ));
    }
}
