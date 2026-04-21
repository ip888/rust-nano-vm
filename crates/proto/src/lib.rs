//! `agent-sandbox-proto` — the wire protocol between a host VMM and a
//! guest-agent running inside a `rust-nano-vm` sandbox.
//!
//! Goals:
//! - Simple JSON-RPC-ish frames (one [`Request`] → one [`Response`]).
//! - Versioned at the envelope level so the host can refuse to talk to a
//!   too-old or too-new guest.
//! - Symmetric across the host and guest crates. Keeping it small and in
//!   one file helps the protocol stay reviewable.
//!
//! The transport (virtio-vsock in v1) is orthogonal; this crate only
//! defines the on-the-wire shape.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

/// Current protocol version. Bump on any backwards-incompatible change.
pub const PROTOCOL_VERSION: u32 = 1;

/// A correlation id chosen by the caller (host). Echoed back in the matching
/// [`Response`] so pipelined requests can be demuxed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(pub u64);

/// Request envelope sent host → guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Protocol version the host is speaking.
    pub version: u32,
    /// Correlation id, echoed in [`Response::id`].
    pub id: RequestId,
    /// Payload.
    pub body: RequestBody,
}

/// Response envelope sent guest → host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    /// Protocol version the guest is speaking.
    pub version: u32,
    /// Correlation id copied from [`Request::id`].
    pub id: RequestId,
    /// Payload (success or error).
    pub result: Result<ResponseBody, RpcError>,
}

/// Supported request operations. Small on purpose in v1 — extend deliberately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RequestBody {
    /// Health check; guest replies with [`ResponseBody::Pong`].
    Ping,
    /// Spawn a process in the guest and collect its output.
    Exec {
        /// Program to execute (absolute path or found on `$PATH`).
        program: String,
        /// Argument vector, NOT including `argv[0]`.
        args: Vec<String>,
        /// Optional working directory inside the guest.
        cwd: Option<String>,
        /// Extra environment variables to set on the child.
        env: Vec<(String, String)>,
        /// Max wall-clock time in milliseconds. `None` means no limit.
        timeout_ms: Option<u64>,
    },
    /// Write a file in the guest filesystem.
    WriteFile {
        /// Absolute path inside the guest.
        path: String,
        /// Raw content.
        content: Vec<u8>,
        /// Permission bits (e.g. 0o644).
        mode: u32,
    },
    /// Read a file from the guest filesystem.
    ReadFile {
        /// Absolute path inside the guest.
        path: String,
    },
    /// Stat a file or directory.
    Stat {
        /// Absolute path inside the guest.
        path: String,
    },
    /// Send a UNIX signal to a previously-spawned process.
    Signal {
        /// Guest-side pid returned from [`ResponseBody::ExecStarted`] (future).
        pid: u32,
        /// Signal number.
        signum: i32,
    },
}

/// Supported response payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponseBody {
    /// Reply to [`RequestBody::Ping`].
    Pong,
    /// Completed [`RequestBody::Exec`] with captured output.
    ExecResult {
        /// Process exit code. `None` if the process was killed by a signal.
        exit_code: Option<i32>,
        /// Signal that terminated the process, if any.
        signal: Option<i32>,
        /// Captured stdout (UTF-8 when possible; raw bytes otherwise).
        stdout: Vec<u8>,
        /// Captured stderr.
        stderr: Vec<u8>,
        /// Wall-clock runtime in milliseconds.
        duration_ms: u64,
    },
    /// Reply to [`RequestBody::WriteFile`]; contains bytes actually written.
    Written {
        /// Bytes actually written.
        bytes: u64,
    },
    /// Reply to [`RequestBody::ReadFile`]; contains file contents.
    FileContent {
        /// File content.
        content: Vec<u8>,
    },
    /// Reply to [`RequestBody::Stat`].
    StatResult {
        /// File size in bytes.
        size: u64,
        /// UNIX permission bits.
        mode: u32,
        /// `true` if the path is a directory.
        is_dir: bool,
    },
    /// Reply to [`RequestBody::Signal`] — empty success.
    SignalSent,
}

/// Error payload returned when an operation fails inside the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    /// Stable machine-readable code.
    pub code: ErrorCode,
    /// Human-readable detail. Not stable; do not match on.
    pub message: String,
}

/// Stable, machine-readable error codes. Add new variants at the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCode {
    /// The host and guest disagree on [`PROTOCOL_VERSION`].
    VersionMismatch,
    /// Malformed or unparseable frame.
    BadRequest,
    /// Requested path does not exist.
    NotFound,
    /// Operation is not permitted by the guest's policy.
    Forbidden,
    /// Generic IO failure inside the guest.
    Io,
    /// Operation exceeded its `timeout_ms`.
    Timeout,
    /// Catch-all for failures the guest cannot classify.
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(v: &T) {
        let json = serde_json::to_string(v).expect("serialize");
        let back: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(v, &back);
    }

    #[test]
    fn ping_request_roundtrips() {
        roundtrip(&Request {
            version: PROTOCOL_VERSION,
            id: RequestId(7),
            body: RequestBody::Ping,
        });
    }

    #[test]
    fn exec_request_roundtrips_with_full_fields() {
        roundtrip(&Request {
            version: PROTOCOL_VERSION,
            id: RequestId(42),
            body: RequestBody::Exec {
                program: "python".into(),
                args: vec!["-c".into(), "print(2+2)".into()],
                cwd: Some("/work".into()),
                env: vec![("RUST_LOG".into(), "info".into())],
                timeout_ms: Some(5000),
            },
        });
    }

    #[test]
    fn exec_result_response_roundtrips() {
        roundtrip(&Response {
            version: PROTOCOL_VERSION,
            id: RequestId(42),
            result: Ok(ResponseBody::ExecResult {
                exit_code: Some(0),
                signal: None,
                stdout: b"4\n".to_vec(),
                stderr: vec![],
                duration_ms: 12,
            }),
        });
    }

    #[test]
    fn error_response_roundtrips() {
        roundtrip(&Response {
            version: PROTOCOL_VERSION,
            id: RequestId(42),
            result: Err(RpcError {
                code: ErrorCode::Timeout,
                message: "exceeded 5000ms".into(),
            }),
        });
    }

    #[test]
    fn request_body_tag_is_op() {
        // Protocol stability: the `op` tag is part of the wire contract.
        let json = serde_json::to_string(&RequestBody::Ping).unwrap();
        assert_eq!(json, r#"{"op":"ping"}"#);
    }

    #[test]
    fn response_body_tag_is_kind() {
        let json = serde_json::to_string(&ResponseBody::Pong).unwrap();
        assert_eq!(json, r#"{"kind":"pong"}"#);
    }
}
