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
//! 1. Accept a stream from the host over AF_VSOCK.
//! 2. Enter a request loop: read a length-prefixed JSON
//!    [`proto::Request`], dispatch to the handler, write back a
//!    length-prefixed [`proto::Response`].
//!
//! The AF_VSOCK socket open/bind/accept step is still blocked on safe socket
//! integration, but the framed request/response path is already implemented:
//! set `NANOVM_AGENT_FRAMED=1` to make the binary use 4-byte little-endian
//! length prefixes on stdin/stdout. That framing matches the intended vsock
//! transport and is unit-testable without KVM.
//!
//! ## Stdin / stdout mode (current, for local testing without KVM)
//!
//! The agent reads newline-delimited JSON requests from `stdin` and writes
//! newline-delimited JSON responses to `stdout`. This makes it trivially
//! testable without a guest:
//!
//! ```sh
//! echo '{"version":1,"id":{"0":1},"body":{"op":"ping"}}' | cargo run -p guest-agent
//! ```
//!
//! ### Streaming exec in stdin/stdout mode
//!
//! `ExecStart` runs the process to completion and emits three response frames
//! before reading the next request:
//! 1. `ExecStarted { pid }` — the pid of the spawned child.
//! 2. Zero or more `ExecOutput` frames for stdout and stderr chunks.
//! 3. `ExecExited { pid, exit_code, signal, duration_ms }` — terminal frame.
//!
//! This is "pseudo-streaming": the output is buffered internally (via
//! `Child::wait_with_output`) and flushed as frames after the child exits.
//! True streaming (emitting output as it arrives) requires async I/O or
//! threads and is deferred to the virtio-vsock wiring.
//!
//! `ExecWait` for a pid that completed during a previous `ExecStart` returns
//! `ExecExited` immediately from the cached exit status.
//!
//! `ExecStdin` and `Signal` for any pid reply with `NoSuchProcess` in
//! sequential mode — there is no concurrently running child.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};
use std::time::Instant;

use proto::{ErrorCode, Request, RequestId, Response, ResponseBody, RpcError, PROTOCOL_VERSION};

// ---------------------------------------------------------------------------
// Agent state
// ---------------------------------------------------------------------------

/// Exit info retained after a child finishes so `ExecWait` can respond.
struct ExitInfo {
    exit_code: Option<i32>,
    signal: Option<i32>,
    duration_ms: u64,
}

/// Mutable state shared across request handlers.
struct AgentState {
    /// pid → exit info for processes that already finished.
    exited: HashMap<u32, ExitInfo>,
}

impl AgentState {
    fn new() -> Self {
        Self {
            exited: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// `VMADDR_CID_HOST` — the well-known vsock context id of the host
/// (the VMM). The agent always connects *out* to the host.
const VMADDR_CID_HOST: u32 = 2;
/// Default host port the agent connects to when `NANOVM_AGENT_VSOCK` is
/// set without an explicit port.
const DEFAULT_VSOCK_PORT: u32 = 1024;

fn main() {
    // Tick mode: a busy-spin loop that periodically logs a monotonically
    // increasing counter. Used by the snapshot/restore test as a guest that
    // (a) stays runnable (never HLT-idles, so the VM doesn't stop) and
    // (b) makes forward progress observable on the serial console — a
    // restored snapshot resumes the counter where it froze.
    if std::env::var_os("NANOVM_AGENT_TICK").is_some() {
        run_tick_loop();
    }

    // The host passes `NANOVM_AGENT_VSOCK[=<port>]` on the kernel
    // cmdline when it attaches a vsock device; the Linux init path
    // hands unknown `key=value` cmdline tokens to PID 1 as environment
    // variables, so the agent picks it up here. When present we use the
    // virtio-vsock transport; otherwise we fall back to the stdio modes
    // that make the agent testable without a guest.
    if let Some(port) = vsock_port_from_env() {
        run_vsock_session(port);
        return;
    }

    let framed = std::env::var_os("NANOVM_AGENT_FRAMED").is_some();
    log_banner(framed);

    let stdin = io::stdin();
    let stdout = io::stdout();
    if framed {
        run_length_prefixed_session(stdin.lock(), stdout.lock());
    } else {
        run_line_delimited_session(stdin.lock(), stdout.lock());
    }
}

/// Parse the host port from `NANOVM_AGENT_VSOCK`. `None` disables vsock
/// mode. A bare/`1`/non-numeric value selects [`DEFAULT_VSOCK_PORT`]; a
/// numeric value sets the port explicitly.
fn vsock_port_from_env() -> Option<u32> {
    let val = std::env::var("NANOVM_AGENT_VSOCK").ok()?;
    Some(val.trim().parse::<u32>().unwrap_or(DEFAULT_VSOCK_PORT))
}

/// Connect to the host over AF_VSOCK and serve the length-prefixed
/// request/response protocol over that stream.
///
/// As the guest's PID 1 the agent must not exit (that panics the
/// kernel), so connect failures and a closed session both fall through
/// to a parked halt loop rather than returning.
fn run_vsock_session(port: u32) {
    use std::time::Duration;
    use vsock::VsockStream;

    kmsg_log(&format!(
        "nanovm-agent: vsock mode (proto v{PROTOCOL_VERSION}, host_cid={VMADDR_CID_HOST}, port={port}, pid={})\n",
        std::process::id(),
    ));

    // The host device + listener are up by the time the guest driver
    // probes, but the agent may race ahead of the host accepting; retry
    // briefly before giving up.
    let mut stream = None;
    for attempt in 1..=50u32 {
        match VsockStream::connect_with_cid_port(VMADDR_CID_HOST, port) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => {
                if attempt == 1 || attempt % 10 == 0 {
                    kmsg_log(&format!(
                        "nanovm-agent: vsock connect attempt {attempt} failed: {e}\n"
                    ));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    let Some(stream) = stream else {
        kmsg_log("nanovm-agent: vsock connect gave up; parking\n");
        park_forever();
    };

    kmsg_log("nanovm-agent: vsock connected; serving requests\n");
    match stream.try_clone() {
        Ok(reader) => run_length_prefixed_session(reader, stream),
        Err(e) => kmsg_log(&format!("nanovm-agent: vsock try_clone failed: {e}\n")),
    }
    kmsg_log("nanovm-agent: vsock session ended; parking\n");
    park_forever();
}

/// Busy-spin loop that logs `nanovm-tick <n>` to `/dev/kmsg` forever.
///
/// Spins (rather than sleeps) between logs so the vCPU never HLT-idles —
/// an idle guest would exit to the host and stop the VM, which a snapshot
/// target must not do. The counter lives in guest memory and the loop's
/// instruction pointer in vCPU state, so a restored snapshot resumes
/// counting from where it was frozen.
fn run_tick_loop() -> ! {
    kmsg_log("nanovm-agent: tick mode\n");
    let mut n: u64 = 0;
    loop {
        let mut x: u64 = 0;
        for i in 0..300_000_000u64 {
            x = x.wrapping_add(i);
        }
        std::hint::black_box(x);
        kmsg_log(&format!("nanovm-tick {n}\n"));
        n = n.wrapping_add(1);
    }
}

/// Sleep indefinitely. Used after the agent's work is done so PID 1
/// never returns (which would panic the guest kernel); the host tears
/// the VM down when it's finished.
fn park_forever() -> ! {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Best-effort log line to the kernel ring buffer (reaches the host
/// serial console) and stderr. Never panics — see [`log_banner`].
fn kmsg_log(line: &str) {
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    }
    let _ = io::stderr().write_all(line.as_bytes());
}

/// Emit the startup banner so the host can confirm the agent
/// launched and learn its protocol version before any request
/// round-trip.
///
/// Routed through `/dev/kmsg` (the kernel ring buffer) first: when
/// the agent runs as the guest's PID 1 in a minimal initramfs, its
/// stdio is often NOT wired to a usable tty, so a plain write to
/// stderr fails — and `eprintln!` *panics* on that failure, which
/// would kill init. Writing to `/dev/kmsg` goes straight through
/// printk to the serial console (the path that reliably reaches the
/// host's serial capture). Both writes are best-effort: a guest
/// without `/dev/kmsg` or a broken stderr must not crash the agent.
fn log_banner(framed: bool) {
    kmsg_log(&format!(
        "nanovm-agent: ready (proto v{}, framed={}, pid={})\n",
        PROTOCOL_VERSION,
        framed,
        std::process::id(),
    ));
}

fn run_line_delimited_session(input: impl BufRead, mut out: impl Write) {
    let mut state = AgentState::new();
    for line in input.lines() {
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
                let resp = error_response(
                    RequestId(0),
                    ErrorCode::BadRequest,
                    format!("malformed request: {e}"),
                );
                if write_response(&mut out, &resp).is_err() {
                    break;
                }
                continue;
            }
        };

        if process_request(req, &mut out, &mut state).is_err() {
            break;
        }
    }
}

fn run_length_prefixed_session(mut input: impl Read, mut out: impl Write) {
    let mut state = AgentState::new();
    loop {
        let frame = match read_frame(&mut input) {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(e) => {
                eprintln!("nanovm-agent: framed read error: {e}");
                break;
            }
        };

        let req: Request = match serde_json::from_slice(&frame) {
            Ok(r) => r,
            Err(e) => {
                let resp = error_response(
                    RequestId(0),
                    ErrorCode::BadRequest,
                    format!("malformed request: {e}"),
                );
                if write_frame(&mut out, &resp).is_err() {
                    break;
                }
                continue;
            }
        };

        let mut output_bytes = Vec::new();
        if process_request(req, &mut output_bytes, &mut state).is_err() {
            break;
        }

        for frame in output_bytes
            .split(|b| *b == b'\n')
            .filter(|part| !part.is_empty())
        {
            let resp: Response = match serde_json::from_slice(frame) {
                Ok(resp) => resp,
                Err(e) => {
                    eprintln!("nanovm-agent: internal framed decode error: {e}");
                    return;
                }
            };
            if write_frame(&mut out, &resp).is_err() {
                return;
            }
        }
    }
}

fn process_request(req: Request, out: &mut impl Write, state: &mut AgentState) -> Result<(), ()> {
    if req.version != PROTOCOL_VERSION {
        let resp = error_response(
            req.id,
            ErrorCode::VersionMismatch,
            format!(
                "expected protocol version {PROTOCOL_VERSION}, got {}",
                req.version
            ),
        );
        return write_response(out, &resp);
    }
    handle_request(req, out, state)
}

// ---------------------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------------------

/// Dispatch one request, writing one or more response frames to `out`.
/// Returns `Err(())` if `out` is broken (the caller should stop the loop).
fn handle_request(req: Request, out: &mut impl Write, state: &mut AgentState) -> Result<(), ()> {
    use proto::RequestBody;
    match req.body {
        RequestBody::ExecStart {
            program,
            args,
            cwd,
            env,
        } => handle_exec_start(req.id, program, args, cwd, env, out, state),
        RequestBody::ExecStdin { pid, .. } => {
            // In sequential stdin/stdout mode there is no concurrently running
            // child to feed: ExecStart blocks until the process exits. Return
            // NoSuchProcess so the host can distinguish "never started" from
            // "still running" (which can't happen here).
            let resp = error_response(
                req.id,
                ErrorCode::NoSuchProcess,
                format!(
                    "no active process {pid}: \
                     ExecStdin is not supported in sequential stdin/stdout mode"
                ),
            );
            write_response(out, &resp)
        }
        RequestBody::ExecWait { pid } => handle_exec_wait(req.id, pid, out, state),
        RequestBody::Signal { pid, .. } => {
            // Same sequencing constraint: no running child to signal.
            let resp = error_response(
                req.id,
                ErrorCode::NoSuchProcess,
                format!(
                    "no active process {pid}: \
                     Signal is not supported in sequential stdin/stdout mode"
                ),
            );
            write_response(out, &resp)
        }
        body => {
            // One-shot handlers: one request → one response.
            let result = dispatch_oneshot(body);
            let resp = Response {
                version: PROTOCOL_VERSION,
                id: req.id,
                result,
            };
            write_response(out, &resp)
        }
    }
}

/// Handle `ExecStart`: spawn the process, emit `ExecStarted`, stream output
/// as `ExecOutput` frames, then emit `ExecExited`. Stores the exit status so
/// a subsequent `ExecWait` can respond immediately.
fn handle_exec_start(
    req_id: RequestId,
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: Vec<(String, String)>,
    out: &mut impl Write,
    state: &mut AgentState,
) -> Result<(), ()> {
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // stdin is not wired in sequential mode
        .stdin(Stdio::null());
    strip_agent_transport_env(&mut cmd);
    if let Some(ref dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in &env {
        cmd.env(k, v);
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let resp = error_response(
                req_id,
                ErrorCode::Io,
                format!("failed to spawn {program}: {e}"),
            );
            return write_response(out, &resp);
        }
    };
    let pid = child.id();
    let start = Instant::now();

    // Announce the child pid before we block waiting for it.
    write_response(
        out,
        &Response {
            version: PROTOCOL_VERSION,
            id: req_id,
            result: Ok(ResponseBody::ExecStarted { pid }),
        },
    )?;

    // Block until the child exits, collecting all output.
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            let resp = error_response(
                req_id,
                ErrorCode::Io,
                format!("wait_with_output for {program}: {e}"),
            );
            return write_response(out, &resp);
        }
    };
    let duration_ms = start.elapsed().as_millis().min(u64::MAX as u128) as u64;

    // Emit stdout as an ExecOutput frame (skip if empty).
    if !output.stdout.is_empty() {
        write_response(
            out,
            &Response {
                version: PROTOCOL_VERSION,
                id: req_id,
                result: Ok(ResponseBody::ExecOutput {
                    pid,
                    stream: proto::StdStream::Stdout,
                    data: output.stdout,
                }),
            },
        )?;
    }
    // Emit stderr as a separate ExecOutput frame (skip if empty).
    if !output.stderr.is_empty() {
        write_response(
            out,
            &Response {
                version: PROTOCOL_VERSION,
                id: req_id,
                result: Ok(ResponseBody::ExecOutput {
                    pid,
                    stream: proto::StdStream::Stderr,
                    data: output.stderr,
                }),
            },
        )?;
    }

    let exit_code = output.status.code();
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        output.status.signal()
    };
    #[cfg(not(unix))]
    let signal: Option<i32> = None;

    // Cache exit info so ExecWait can respond.
    state.exited.insert(
        pid,
        ExitInfo {
            exit_code,
            signal,
            duration_ms,
        },
    );

    // Terminal frame.
    write_response(
        out,
        &Response {
            version: PROTOCOL_VERSION,
            id: req_id,
            result: Ok(ResponseBody::ExecExited {
                pid,
                exit_code,
                signal,
                duration_ms,
            }),
        },
    )
}

/// Handle `ExecWait`: if the process has already exited (via `ExecStart`
/// in this session) reply with `ExecExited`; otherwise `NoSuchProcess`.
fn handle_exec_wait(
    req_id: RequestId,
    pid: u32,
    out: &mut impl Write,
    state: &mut AgentState,
) -> Result<(), ()> {
    match state.exited.get(&pid) {
        Some(info) => write_response(
            out,
            &Response {
                version: PROTOCOL_VERSION,
                id: req_id,
                result: Ok(ResponseBody::ExecExited {
                    pid,
                    exit_code: info.exit_code,
                    signal: info.signal,
                    duration_ms: info.duration_ms,
                }),
            },
        ),
        None => {
            let resp = error_response(
                req_id,
                ErrorCode::NoSuchProcess,
                format!("pid {pid} not found — it was never started or already reaped"),
            );
            write_response(out, &resp)
        }
    }
}

// ---------------------------------------------------------------------------
// One-shot handlers (one request → one response)
// ---------------------------------------------------------------------------

/// Route a one-shot request body to the appropriate handler. Every variant
/// here produces exactly one `Response`.
fn dispatch_oneshot(body: proto::RequestBody) -> Result<ResponseBody, RpcError> {
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
        // Streaming variants are handled by `handle_request` before reaching
        // here and should never appear in the oneshot path.
        _ => Err(RpcError {
            code: ErrorCode::BadRequest,
            message: "unexpected streaming op in one-shot path".into(),
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
    use std::process::{Command, Stdio};

    let start = Instant::now();

    let mut cmd = Command::new(&program);
    cmd.args(&args).stdin(Stdio::null());
    strip_agent_transport_env(&mut cmd);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }

    let output = cmd.output().map_err(|e| RpcError {
        code: ErrorCode::Io,
        message: format!("failed to spawn {program}: {e}"),
    })?;

    let duration_ms = start.elapsed().as_millis().min(u64::MAX as u128) as u64;

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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Remove the agent's own transport-selection env vars from a child
/// command's environment. Guest processes the agent spawns must not
/// inherit these — otherwise exec'ing the agent binary (or any program
/// that checks them) would re-enter vsock mode and connect back to the
/// host, which is never what the caller wants.
fn strip_agent_transport_env(cmd: &mut std::process::Command) {
    cmd.env_remove("NANOVM_AGENT_VSOCK");
    cmd.env_remove("NANOVM_AGENT_FRAMED");
}

fn error_response(id: RequestId, code: ErrorCode, message: String) -> Response {
    Response {
        version: PROTOCOL_VERSION,
        id,
        result: Err(RpcError { code, message }),
    }
}

fn write_response(out: &mut impl Write, resp: &Response) -> Result<(), ()> {
    let json = match serde_json::to_string(resp) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("nanovm-agent: serialize error: {e}");
            return Err(());
        }
    };
    writeln!(out, "{json}").map_err(|_| ())?;
    out.flush().map_err(|_| ())
}

fn read_frame(input: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    /// Maximum allowed length for a single framed request, to prevent an
    /// out-of-memory condition caused by a malicious or corrupted length field.
    const MAX_FRAME_LEN: usize = 16 * 1024 * 1024; // 16 MiB

    let mut len_buf = [0u8; 4];
    match input.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds maximum {MAX_FRAME_LEN} bytes"),
        ));
    }
    let mut frame = vec![0u8; len];
    input.read_exact(&mut frame)?;
    Ok(Some(frame))
}

fn write_frame(out: &mut impl Write, resp: &Response) -> Result<(), ()> {
    let json = match serde_json::to_vec(resp) {
        Ok(buf) => buf,
        Err(e) => {
            eprintln!("nanovm-agent: serialize error: {e}");
            return Err(());
        }
    };
    let len = u32::try_from(json.len()).map_err(|_| ())?;
    out.write_all(&len.to_le_bytes()).map_err(|_| ())?;
    out.write_all(&json).map_err(|_| ())?;
    out.flush().map_err(|_| ())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use proto::RequestBody;

    fn run_oneshot(body: RequestBody) -> Result<ResponseBody, RpcError> {
        dispatch_oneshot(body)
    }

    #[test]
    fn ping_returns_pong() {
        let result = run_oneshot(RequestBody::Ping);
        assert!(matches!(result, Ok(ResponseBody::Pong)));
    }

    #[test]
    fn exec_echo_returns_exit_zero_and_captures_stdout() {
        let result = run_oneshot(RequestBody::Exec {
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
        let result = run_oneshot(RequestBody::Exec {
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

        run_oneshot(RequestBody::WriteFile {
            path: path.clone(),
            content: content.clone(),
            mode: 0o644,
        })
        .unwrap();

        let result = run_oneshot(RequestBody::ReadFile { path: path.clone() });
        match result {
            Ok(ResponseBody::FileContent { content: got }) => assert_eq!(got, content),
            other => panic!("expected FileContent, got {other:?}"),
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stat_missing_file_returns_not_found() {
        let result = run_oneshot(RequestBody::Stat {
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
        let json = serde_json::to_string(&req).unwrap();
        let mut buf = json.as_bytes().to_vec();
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
                dispatch_oneshot(req.body)
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

    #[test]
    fn framed_session_roundtrips_ping() {
        let req = Request {
            version: PROTOCOL_VERSION,
            id: RequestId(7),
            body: RequestBody::Ping,
        };
        let payload = serde_json::to_vec(&req).unwrap();
        let mut input = Vec::new();
        input.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        input.extend_from_slice(&payload);

        let mut out = Vec::new();
        run_length_prefixed_session(io::Cursor::new(input), &mut out);

        let mut cursor = io::Cursor::new(out);
        let frame = read_frame(&mut cursor).unwrap().unwrap();
        let resp: Response = serde_json::from_slice(&frame).unwrap();
        assert_eq!(resp.id, RequestId(7));
        assert!(matches!(resp.result, Ok(ResponseBody::Pong)));
    }

    #[test]
    fn framed_session_returns_bad_request_for_malformed_json() {
        let bad = b"{ definitely-not-json".to_vec();
        let mut input = Vec::new();
        input.extend_from_slice(&(bad.len() as u32).to_le_bytes());
        input.extend_from_slice(&bad);

        let mut out = Vec::new();
        run_length_prefixed_session(io::Cursor::new(input), &mut out);

        let mut cursor = io::Cursor::new(out);
        let frame = read_frame(&mut cursor).unwrap().unwrap();
        let resp: Response = serde_json::from_slice(&frame).unwrap();
        assert!(matches!(
            resp.result,
            Err(RpcError {
                code: ErrorCode::BadRequest,
                ..
            })
        ));
    }

    // ---- Streaming exec -------------------------------------------------

    /// Run `handle_exec_start` against a real command, collect the frames.
    fn exec_start(program: &str, args: &[&str]) -> (Vec<ResponseBody>, AgentState) {
        let mut out = Vec::new();
        let mut state = AgentState::new();
        handle_exec_start(
            proto::RequestId(1),
            program.into(),
            args.iter().map(|s| s.to_string()).collect(),
            None,
            vec![],
            &mut out,
            &mut state,
        )
        .expect("handle_exec_start ok");

        let frames: Vec<ResponseBody> = String::from_utf8(out)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| {
                let r: Response = serde_json::from_str(l).unwrap();
                r.result.unwrap()
            })
            .collect();
        (frames, state)
    }

    #[test]
    fn exec_start_emits_started_output_exited() {
        let (frames, mut state) = exec_start("echo", &["streaming"]);

        // Frame 0: ExecStarted
        let pid = match &frames[0] {
            ResponseBody::ExecStarted { pid } => *pid,
            f => panic!("expected ExecStarted, got {f:?}"),
        };
        assert!(pid > 0);

        // Last frame: ExecExited
        let last = frames.last().unwrap();
        assert!(
            matches!(
                last,
                ResponseBody::ExecExited {
                    exit_code: Some(0),
                    ..
                }
            ),
            "got {last:?}"
        );

        // ExecWait should resolve immediately from the cached state
        let mut dummy_out = Vec::new();
        handle_exec_wait(proto::RequestId(2), pid, &mut dummy_out, &mut state).expect("wait ok");
        let resp: Response = serde_json::from_slice(&dummy_out).unwrap();
        assert!(matches!(
            resp.result,
            Ok(ResponseBody::ExecExited {
                exit_code: Some(0),
                ..
            })
        ));
    }

    #[test]
    fn exec_start_non_zero_exit_reflected_in_exited_frame() {
        let (frames, _) = exec_start("sh", &["-c", "exit 42"]);
        let last = frames.last().unwrap();
        assert!(
            matches!(
                last,
                ResponseBody::ExecExited {
                    exit_code: Some(42),
                    ..
                }
            ),
            "got {last:?}"
        );
    }

    #[test]
    fn exec_start_bad_binary_returns_error_frame() {
        let mut out = Vec::new();
        let mut state = AgentState::new();
        handle_exec_start(
            proto::RequestId(1),
            "/no/such/binary".into(),
            vec![],
            None,
            vec![],
            &mut out,
            &mut state,
        )
        .expect("write ok");

        let resp: Response = serde_json::from_slice(&out).unwrap();
        assert!(matches!(
            resp.result,
            Err(RpcError {
                code: ErrorCode::Io,
                ..
            })
        ));
    }

    #[test]
    fn exec_wait_unknown_pid_returns_no_such_process() {
        let mut out = Vec::new();
        let mut state = AgentState::new();
        handle_exec_wait(proto::RequestId(1), 999_999, &mut out, &mut state).expect("write ok");
        let resp: Response = serde_json::from_slice(&out).unwrap();
        assert!(matches!(
            resp.result,
            Err(RpcError {
                code: ErrorCode::NoSuchProcess,
                ..
            })
        ));
    }
}
