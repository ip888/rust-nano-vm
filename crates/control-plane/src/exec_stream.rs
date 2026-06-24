//! Server-Sent Events handler for streaming guest exec.
//!
//! `POST /v1/vms/:id/exec/stream` is the streaming counterpart of
//! `POST /v1/vms/:id/exec`. The request body is identical; the
//! response is `text/event-stream` with three event types, in this
//! order, terminated by an `exit` event:
//!
//! ```text
//! event: stdout
//! data: <base64-encoded bytes>
//!
//! event: stderr
//! data: <base64-encoded bytes>
//!
//! event: exit
//! data: {"exit_code":0,"signal":null,"duration_ms":12}
//! ```
//!
//! `stdout` / `stderr` chunk boundaries follow the underlying
//! transport — clients must treat the stream as a byte stream, not a
//! line stream. The `exit` event is always last; clients should close
//! the connection on receipt.
//!
//! Cancellation: dropping the client connection drops the SSE
//! receiver, which makes subsequent channel sends fail in the
//! producer task. The producer exits its loop on the next frame, but
//! the underlying child process is NOT killed — kill behaviour will
//! land when streaming exec is wired through `vm-kvm`'s guest agent.

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    extract::{
        rejection::{JsonRejection, PathRejection},
        Path, State,
    },
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::Serialize;
use tokio_stream::wrappers::ReceiverStream;
use vm_core::{ExecFrame, VmId};

/// Bound on the in-flight SSE event buffer per client. A bounded
/// channel (vs the previous `unbounded_channel`) is what gives us
/// backpressure: when a slow client can't read events as fast as the
/// backend produces them, the spawn_blocking producer parks on
/// `blocking_send` instead of growing an unbounded queue and OOM'ing
/// the host. 64 holds about a megabyte of base64'd stdout in the
/// worst case (each event ≤ a few KiB), which is plenty for typical
/// bursts without blocking the producer on every chunk.
const SSE_BUFFER: usize = 64;

use crate::api::ExecRequest;
use crate::error::ApiError;
use crate::routes::AppState;

/// Wire shape of the terminal `exit` event payload.
#[derive(Debug, Serialize)]
struct ExitPayload {
    exit_code: Option<i32>,
    signal: Option<i32>,
    duration_ms: u64,
}

/// SSE handler for `POST /v1/vms/:id/exec/stream`.
///
/// The body parses as the existing `ExecRequest` so the wire shape
/// stays 1:1 with the non-streaming `/exec` endpoint. Headers and
/// auth are inherited from the `/v1/*` middleware stack — same
/// bearer-token gate and audit row as every other mutating call.
pub(crate) async fn exec_vm_stream(
    State(state): State<AppState>,
    id: Result<Path<u64>, PathRejection>,
    body: Result<Json<ExecRequest>, JsonRejection>,
) -> Result<Sse<ReceiverStream<Result<Event, Infallible>>>, ApiError> {
    let Path(id) = id?;
    let Json(req) = body?;

    // Spawn the backend's blocking stream BEFORE we return the SSE
    // response — that way validation errors (UnknownVm,
    // InvalidTransition, Unsupported) surface as a normal JSON error
    // envelope with the correct status, not as an SSE stream that
    // immediately closes.
    let mut stream = state
        .hypervisor()
        .exec_in_guest_stream(VmId(id), req.into())?;

    // Bounded channel + `blocking_send` is the backpressure path:
    // the producer parks when the SSE consumer can't keep up,
    // capping in-flight memory at `SSE_BUFFER` events.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(SSE_BUFFER);

    // Pull frames on the blocking pool — `ExecStream::next_frame` is
    // synchronous and can block on guest IO for arbitrary durations.
    tokio::task::spawn_blocking(move || loop {
        match stream.next_frame() {
            Ok(Some(ExecFrame::Stdout(bytes))) => {
                let event = Event::default().event("stdout").data(B64.encode(&bytes));
                if tx.blocking_send(Ok(event)).is_err() {
                    return;
                }
            }
            Ok(Some(ExecFrame::Stderr(bytes))) => {
                let event = Event::default().event("stderr").data(B64.encode(&bytes));
                if tx.blocking_send(Ok(event)).is_err() {
                    return;
                }
            }
            Ok(Some(ExecFrame::Exit {
                exit_code,
                signal,
                duration_ms,
            })) => {
                let payload = serde_json::to_string(&ExitPayload {
                    exit_code,
                    signal,
                    duration_ms,
                })
                .unwrap_or_else(|_| "{}".into());
                let event = Event::default().event("exit").data(payload);
                let _ = tx.blocking_send(Ok(event));
                return;
            }
            Ok(None) => {
                // The backend stream ended without emitting a
                // terminal `Exit` — that's a contract violation.
                // Surface it as an `error` event so the client
                // doesn't treat the closed connection as a clean
                // successful completion (the SSE wire docs say
                // "exit is always last"). Without this, a Python
                // SDK consumer would exit its `for event in
                // exec_stream(...)` loop without ever observing an
                // `ExecExit`.
                let event = Event::default()
                    .event("error")
                    .data("backend ended exec stream without an exit frame");
                let _ = tx.blocking_send(Ok(event));
                return;
            }
            Err(e) => {
                // Surface backend errors as an `error` event so the
                // client gets *something* rather than a silent close.
                let event = Event::default().event("error").data(format!("{e}"));
                let _ = tx.blocking_send(Ok(event));
                return;
            }
        }
    });

    Ok(Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}
