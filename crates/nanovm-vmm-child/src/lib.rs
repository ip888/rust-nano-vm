//! Server logic for the `nanovm-vmm-child` worker.
//!
//! Exposed as a library so integration tests can drive the
//! `serve()` loop directly over a `tokio::io::duplex` channel
//! without spawning the binary. The binary's `main.rs` is the
//! production caller — it binds a `UnixListener` to a path the
//! orchestrator gave us, accepts one connection, and hands the
//! split read/write halves to `serve()`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use vm_core::Hypervisor;
use vmm_ipc::framing::{read_frame, write_frame, FrameError};
use vmm_ipc::{Request, Response};

/// Run the request/response loop over a connected stream pair until
/// the peer sends [`Request::Shutdown`] or the transport closes.
///
/// Errors here are *transport* errors: the orchestrator went away,
/// the wire got corrupted, the JSON didn't parse. Semantic errors
/// (unknown VM, invalid transition, etc.) round-trip back to the
/// peer as [`Response::Error`] frames and do not terminate the
/// loop.
pub async fn serve<R, W>(
    hv: Arc<dyn Hypervisor>,
    mut reader: R,
    mut writer: W,
) -> Result<(), FrameError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let req = match read_frame::<_, Request>(&mut reader).await {
            Ok(r) => r,
            // A clean disconnect (EOF after a complete shutdown
            // round-trip) lands here too. The caller treats both
            // shapes the same: the worker exits.
            Err(FrameError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                tracing::info!("peer disconnected, exiting serve loop");
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let is_shutdown = matches!(req, Request::Shutdown);
        tracing::debug!(?req, "request");
        let resp = dispatch(&hv, req);
        if resp.is_error() {
            tracing::debug!(?resp, "response (error)");
        } else {
            tracing::trace!(?resp, "response");
        }
        write_frame(&mut writer, &resp).await?;
        if is_shutdown {
            tracing::info!("shutdown requested, exiting serve loop");
            return Ok(());
        }
    }
}

/// Translate one [`Request`] into a [`Response`] by calling the
/// matching method on the worker's [`Hypervisor`].
///
/// Pure dispatcher — every backend call is synchronous (mock and
/// real-KVM operations are short and CPU-bound) so we don't need a
/// per-request `spawn`. Async hand-off can be added in a later
/// milestone if a backend grows truly blocking work.
pub fn dispatch(hv: &Arc<dyn Hypervisor>, req: Request) -> Response {
    match req {
        Request::Ping => Response::Pong,
        // Shutdown is a sentinel: dispatch produces the reply,
        // serve() decides to break the loop based on the request
        // kind it saw before calling us.
        Request::Shutdown => Response::Empty,
        Request::CreateVm { config } => match hv.create_vm(&config) {
            Ok(handle) => Response::VmHandle(handle),
            Err(e) => (&e).into(),
        },
        Request::Start { id } => match hv.start(id) {
            Ok(()) => Response::Empty,
            Err(e) => (&e).into(),
        },
        Request::Stop { id } => match hv.stop(id) {
            Ok(()) => Response::Empty,
            Err(e) => (&e).into(),
        },
        Request::Snapshot { id } => match hv.snapshot(id) {
            Ok(snap_id) => Response::Snapshot { id: snap_id },
            Err(e) => (&e).into(),
        },
        Request::Restore { id } => match hv.restore(id) {
            Ok(handle) => Response::VmHandle(handle),
            Err(e) => (&e).into(),
        },
        Request::Destroy { id } => match hv.destroy(id) {
            Ok(()) => Response::Empty,
            Err(e) => (&e).into(),
        },
        Request::State { id } => match hv.state(id) {
            Ok(state) => Response::State { state },
            Err(e) => (&e).into(),
        },
        Request::VmMeta { id } => match hv.vm_meta(id) {
            Ok(meta) => Response::VmMeta(meta),
            Err(e) => (&e).into(),
        },
        Request::SnapshotMeta { id } => match hv.snapshot_meta(id) {
            Ok(meta) => Response::SnapshotMeta(meta),
            Err(e) => (&e).into(),
        },
        Request::DeleteSnapshot { id } => match hv.delete_snapshot(id) {
            Ok(()) => Response::Empty,
            Err(e) => (&e).into(),
        },
        Request::ListSnapshots => match hv.list_snapshots() {
            Ok(ids) => Response::SnapshotIds { ids },
            Err(e) => (&e).into(),
        },
        Request::ExecInGuest { id, req: exec_req } => match hv.exec_in_guest(id, exec_req) {
            Ok(result) => Response::ExecResult(result),
            Err(e) => (&e).into(),
        },
        Request::ReadFile { id, path } => match hv.read_file(id, path) {
            Ok(content) => Response::Bytes { content },
            Err(e) => (&e).into(),
        },
        Request::WriteFile {
            id,
            path,
            content,
            mode,
        } => match hv.write_file(id, path, content, mode) {
            Ok(bytes) => Response::Written { bytes },
            Err(e) => (&e).into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_core::VmConfig;
    use vm_mock::MockHypervisor;

    fn mock() -> Arc<dyn Hypervisor> {
        Arc::new(MockHypervisor::new())
    }

    #[test]
    fn ping_returns_pong() {
        let r = dispatch(&mock(), Request::Ping);
        assert!(matches!(r, Response::Pong));
    }

    #[test]
    fn shutdown_returns_empty() {
        let r = dispatch(&mock(), Request::Shutdown);
        assert!(matches!(r, Response::Empty));
    }

    #[test]
    fn create_then_start_then_stop_then_destroy_roundtrip_yields_typed_responses() {
        let hv = mock();
        let r = dispatch(
            &hv,
            Request::CreateVm {
                config: VmConfig::default(),
            },
        );
        let id = match r {
            Response::VmHandle(h) => h.id,
            other => panic!("expected VmHandle, got {other:?}"),
        };
        assert!(matches!(
            dispatch(&hv, Request::Start { id }),
            Response::Empty
        ));
        assert!(matches!(
            dispatch(&hv, Request::Stop { id }),
            Response::Empty
        ));
        assert!(matches!(
            dispatch(&hv, Request::Destroy { id }),
            Response::Empty
        ));
    }

    #[test]
    fn unknown_vm_surfaces_as_response_error_unknown_vm() {
        let hv = mock();
        let r = dispatch(
            &hv,
            Request::Start {
                id: vm_core::VmId(9999),
            },
        );
        assert!(matches!(
            r,
            Response::Error {
                code: vmm_ipc::ErrorCode::UnknownVm,
                ..
            }
        ));
    }

    #[test]
    fn invalid_transition_surfaces_as_response_error() {
        let hv = mock();
        let r = dispatch(
            &hv,
            Request::CreateVm {
                config: VmConfig::default(),
            },
        );
        let id = match r {
            Response::VmHandle(h) => h.id,
            other => panic!("expected VmHandle, got {other:?}"),
        };
        // Start, then start again — second is invalid.
        let _ = dispatch(&hv, Request::Start { id });
        let r = dispatch(&hv, Request::Start { id });
        assert!(matches!(
            r,
            Response::Error {
                code: vmm_ipc::ErrorCode::InvalidTransition,
                ..
            }
        ));
    }
}
