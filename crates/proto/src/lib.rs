//! `agent-sandbox-proto` — the wire protocol between a host VMM and a
//! guest-agent running inside a `rust-nano-vm` sandbox.
//!
//! Goals:
//! - Simple JSON-RPC-ish frames. Most ops are one [`Request`] → one
//!   [`Response`]; the streaming `ExecStart` op (see below) is the lone
//!   exception and produces multiple responses that share the request's id.
//! - Versioned at the envelope level so the host can refuse to talk to a
//!   too-old or too-new guest.
//! - Symmetric across the host and guest crates. Keeping it small and in
//!   one file helps the protocol stay reviewable.
//!
//! The transport (virtio-vsock in v1) is orthogonal; this crate only
//! defines the on-the-wire shape.
//!
//! # One-shot vs streaming exec
//!
//! [`RequestBody::Exec`] is the simple form: send a request, get one
//! [`ResponseBody::ExecResult`] back when the process has exited, stdout and
//! stderr collected in full. Good for short commands where buffering is fine.
//!
//! [`RequestBody::ExecStart`] is the streaming form (M2). The guest replies
//! immediately with [`ResponseBody::ExecStarted`] carrying the child pid,
//! then pushes [`ResponseBody::ExecOutput`] frames as stdout/stderr arrive,
//! and finally one [`ResponseBody::ExecExited`]. The host correlates these
//! frames by `pid`. All frames share the `id` of the originating `ExecStart`
//! request — so the existing [`Request`]/[`Response`] envelope is preserved,
//! but a single `ExecStart` produces multiple responses.
//!
//! [`RequestBody::ExecStdin`] forwards bytes to a running child's stdin; the
//! guest acknowledges with [`ResponseBody::StdinAccepted`]. If the pid is
//! unknown or already reaped the guest replies with
//! [`ErrorCode::NoSuchProcess`].
//!
//! [`RequestBody::ExecWait`] blocks host-side until the guest emits the
//! terminal [`ResponseBody::ExecExited`] frame for that request. `ExecWait`
//! is independent of the `ExecStart` stream: a single child's exit can
//! produce one `ExecExited` per subscribed request — typically one
//! terminating the `ExecStart` stream plus one for each outstanding
//! `ExecWait` — each carrying that request's own `id`. Hosts correlate by
//! [`Response::id`], not by pid.

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
        /// Guest-side pid returned by [`ResponseBody::ExecStarted`].
        pid: u32,
        /// Signal number.
        signum: i32,
    },
    /// Spawn a process in the guest and stream its output back as
    /// [`ResponseBody::ExecOutput`] frames until [`ResponseBody::ExecExited`]
    /// terminates the stream. Unlike [`RequestBody::Exec`] this does not
    /// buffer stdout/stderr — use it for long-running or chatty commands.
    ExecStart {
        /// Program to execute (absolute path or found on `$PATH`).
        program: String,
        /// Argument vector, NOT including `argv[0]`.
        args: Vec<String>,
        /// Optional working directory inside the guest.
        cwd: Option<String>,
        /// Extra environment variables to set on the child.
        env: Vec<(String, String)>,
    },
    /// Forward bytes to the stdin of a running child spawned by
    /// [`RequestBody::ExecStart`]. Set `eof = true` to close the pipe.
    ExecStdin {
        /// pid returned by [`ResponseBody::ExecStarted`].
        pid: u32,
        /// Raw bytes to write. May be empty when `eof = true`.
        data: Vec<u8>,
        /// If `true`, close the child's stdin after writing `data`.
        eof: bool,
    },
    /// Block until the child identified by `pid` exits. Replies with exactly
    /// one [`ResponseBody::ExecExited`] carrying this request's `id`.
    ///
    /// If the child has already exited at the time this request arrives, the
    /// guest MUST keep its exit status cached long enough to answer (at least
    /// until the host acknowledges; unbounded retention is up to the guest
    /// policy) and reply immediately.
    ///
    /// Callers may use `ExecWait` independently of the `ExecStart` stream —
    /// in that case a single child's exit can produce two `ExecExited`
    /// frames, one per originating request id (one terminating the stream
    /// opened by `ExecStart`, one replying to this `ExecWait`). The host
    /// correlates by the enclosing [`Response::id`].
    ExecWait {
        /// pid returned by [`ResponseBody::ExecStarted`].
        pid: u32,
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
    /// Reply to [`RequestBody::ExecStart`]. The host should stash `pid` and
    /// expect a stream of [`ResponseBody::ExecOutput`] frames ending in
    /// [`ResponseBody::ExecExited`].
    ExecStarted {
        /// Guest-side pid.
        pid: u32,
    },
    /// Streamed stdout/stderr chunk for a child spawned by
    /// [`RequestBody::ExecStart`]. Multiple of these frames share the `id`
    /// of the originating `ExecStart` request; the host correlates by pid.
    ExecOutput {
        /// pid of the child producing the output.
        pid: u32,
        /// Which stream this chunk belongs to.
        stream: StdStream,
        /// Raw bytes (UTF-8 when possible; raw otherwise).
        data: Vec<u8>,
    },
    /// Terminal frame for the request whose `id` this response carries —
    /// either an [`RequestBody::ExecStart`] stream or an
    /// [`RequestBody::ExecWait`]. Once a request has received its
    /// `ExecExited`, no further frames will carry that request's `id`.
    ///
    /// A single child's exit produces one `ExecExited` per subscribed
    /// request: typically one terminating the `ExecStart` stream, plus one
    /// for each outstanding `ExecWait`. Hosts correlate by the enclosing
    /// [`Response::id`].
    ExecExited {
        /// pid of the exited child.
        pid: u32,
        /// Process exit code. `None` if the process was killed by a signal.
        exit_code: Option<i32>,
        /// Signal that terminated the process, if any.
        signal: Option<i32>,
        /// Wall-clock runtime in milliseconds.
        duration_ms: u64,
    },
    /// Reply to [`RequestBody::ExecStdin`] confirming bytes accepted.
    StdinAccepted {
        /// Bytes written to the child's stdin.
        bytes: u64,
    },
}

/// One of the child's standard output streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StdStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
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
    /// Referenced pid does not name a child spawned in this session (or it
    /// has already exited and been reaped).
    NoSuchProcess,
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

    #[test]
    fn exec_start_request_roundtrips() {
        roundtrip(&Request {
            version: PROTOCOL_VERSION,
            id: RequestId(100),
            body: RequestBody::ExecStart {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "while true; do echo hi; sleep 1; done".into()],
                cwd: Some("/work".into()),
                env: vec![],
            },
        });
    }

    #[test]
    fn exec_stdin_request_roundtrips() {
        roundtrip(&Request {
            version: PROTOCOL_VERSION,
            id: RequestId(101),
            body: RequestBody::ExecStdin {
                pid: 4242,
                data: b"line 1\nline 2\n".to_vec(),
                eof: false,
            },
        });
    }

    #[test]
    fn exec_wait_request_roundtrips() {
        roundtrip(&Request {
            version: PROTOCOL_VERSION,
            id: RequestId(102),
            body: RequestBody::ExecWait { pid: 4242 },
        });
    }

    #[test]
    fn exec_started_response_roundtrips() {
        roundtrip(&Response {
            version: PROTOCOL_VERSION,
            id: RequestId(100),
            result: Ok(ResponseBody::ExecStarted { pid: 4242 }),
        });
    }

    #[test]
    fn exec_output_and_exited_roundtrip() {
        roundtrip(&Response {
            version: PROTOCOL_VERSION,
            id: RequestId(100),
            result: Ok(ResponseBody::ExecOutput {
                pid: 4242,
                stream: StdStream::Stdout,
                data: b"hello\n".to_vec(),
            }),
        });
        roundtrip(&Response {
            version: PROTOCOL_VERSION,
            id: RequestId(100),
            result: Ok(ResponseBody::ExecOutput {
                pid: 4242,
                stream: StdStream::Stderr,
                data: b"warn: deprecated flag\n".to_vec(),
            }),
        });
        roundtrip(&Response {
            version: PROTOCOL_VERSION,
            id: RequestId(100),
            result: Ok(ResponseBody::ExecExited {
                pid: 4242,
                exit_code: Some(0),
                signal: None,
                duration_ms: 1234,
            }),
        });
    }

    #[test]
    fn stdin_accepted_roundtrips() {
        roundtrip(&Response {
            version: PROTOCOL_VERSION,
            id: RequestId(101),
            result: Ok(ResponseBody::StdinAccepted { bytes: 14 }),
        });
    }

    #[test]
    fn new_request_variants_serialize_with_stable_tags() {
        // Pin wire-format tags — changing these is a protocol break.
        let json = serde_json::to_string(&RequestBody::ExecStart {
            program: "x".into(),
            args: vec![],
            cwd: None,
            env: vec![],
        })
        .unwrap();
        assert!(json.starts_with(r#"{"op":"exec_start""#), "got: {json}");

        let json = serde_json::to_string(&RequestBody::ExecStdin {
            pid: 1,
            data: vec![],
            eof: true,
        })
        .unwrap();
        assert!(json.starts_with(r#"{"op":"exec_stdin""#), "got: {json}");

        let json = serde_json::to_string(&RequestBody::ExecWait { pid: 1 }).unwrap();
        assert_eq!(json, r#"{"op":"exec_wait","pid":1}"#);
    }

    #[test]
    fn new_response_variants_serialize_with_stable_tags() {
        let json = serde_json::to_string(&ResponseBody::ExecStarted { pid: 42 }).unwrap();
        assert_eq!(json, r#"{"kind":"exec_started","pid":42}"#);

        let json = serde_json::to_string(&ResponseBody::ExecOutput {
            pid: 42,
            stream: StdStream::Stdout,
            data: vec![b'x'],
        })
        .unwrap();
        assert!(
            json.contains(r#""kind":"exec_output""#) && json.contains(r#""stream":"stdout""#),
            "got: {json}"
        );

        let json = serde_json::to_string(&ResponseBody::ExecExited {
            pid: 42,
            exit_code: Some(0),
            signal: None,
            duration_ms: 7,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"kind":"exec_exited","pid":42,"exit_code":0,"signal":null,"duration_ms":7}"#
        );

        let json = serde_json::to_string(&ResponseBody::StdinAccepted { bytes: 14 }).unwrap();
        assert_eq!(json, r#"{"kind":"stdin_accepted","bytes":14}"#);

        // Also pin StdStream's on-the-wire form independently so future code
        // that serializes it standalone doesn't silently shift.
        assert_eq!(
            serde_json::to_string(&StdStream::Stdout).unwrap(),
            r#""stdout""#
        );
        assert_eq!(
            serde_json::to_string(&StdStream::Stderr).unwrap(),
            r#""stderr""#
        );
    }

    #[test]
    fn no_such_process_error_code_has_stable_tag() {
        // `no_such_process_error_roundtrips` only exercises Serialize↔Deserialize
        // symmetry — a lock-step tag rename would still round-trip. Pin the
        // on-the-wire spelling explicitly so external consumers can match on it.
        assert_eq!(
            serde_json::to_string(&ErrorCode::NoSuchProcess).unwrap(),
            r#""no_such_process""#
        );
        let json = serde_json::to_string(&RpcError {
            code: ErrorCode::NoSuchProcess,
            message: "pid 4242 not found".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"code":"no_such_process","message":"pid 4242 not found"}"#
        );
    }

    #[test]
    fn no_such_process_error_roundtrips() {
        roundtrip(&Response {
            version: PROTOCOL_VERSION,
            id: RequestId(102),
            result: Err(RpcError {
                code: ErrorCode::NoSuchProcess,
                message: "pid 4242 not found".into(),
            }),
        });
    }
}
