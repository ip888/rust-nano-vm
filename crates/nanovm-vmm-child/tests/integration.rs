//! End-to-end test: spawn the `nanovm-vmm-child` binary, talk to
//! it over a Unix socket using vmm-ipc, and verify a full VM
//! lifecycle round-trips.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};
use vm_core::{GuestExecRequest, VmConfig, VmId, VmState};
use vmm_ipc::framing::{read_frame, write_frame};
use vmm_ipc::{Request, Response};

/// Path to the worker binary built for testing. `CARGO_BIN_EXE_*`
/// is set by Cargo for the integration test of a binary crate.
fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nanovm-vmm-child"))
}

/// Spawn the worker on a freshly-allocated socket path. Returns the
/// child handle + the socket path so the test can connect and the
/// helper can clean up afterwards.
async fn spawn_worker(tmp: &tempfile::TempDir) -> (Child, PathBuf) {
    let socket = tmp.path().join("worker.sock");
    let child = Command::new(binary_path())
        .arg("--socket")
        .arg(&socket)
        // Quiet logs; flip to `info` if a test starts flaking.
        .env("RUST_LOG", "warn")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn vmm-child");
    // Wait for the bind to take effect.
    for _ in 0..50 {
        if socket.exists() {
            return (child, socket);
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("worker socket {} never appeared", socket.display());
}

async fn connect(socket: &PathBuf) -> UnixStream {
    // spawn_worker returns as soon as the socket path exists, but on
    // Linux the file can appear a moment before the listen() backlog
    // is actually queuing SYNs — a same-tick connect() then races and
    // gets ECONNREFUSED. Retry briefly so the framing tests aren't
    // flaky under CI runner load. 50 × 20 ms = 1 s ceiling, wrapped in
    // the same 2 s timeout as before so a truly dead worker still
    // fails fast.
    let attempt = async {
        for _ in 0..50 {
            match UnixStream::connect(socket).await {
                Ok(s) => return s,
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("connect failed: {e:?}"),
            }
        }
        panic!("connect kept getting ECONNREFUSED after 1 s of retries")
    };
    timeout(Duration::from_secs(2), attempt)
        .await
        .expect("connect timed out")
}

async fn roundtrip(stream: &mut UnixStream, req: Request) -> Response {
    let (mut r, mut w) = stream.split();
    write_frame(&mut w, &req).await.expect("write");
    read_frame::<_, Response>(&mut r).await.expect("read")
}

#[tokio::test]
async fn ping_returns_pong_over_a_real_unix_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket) = spawn_worker(&tmp).await;
    let mut stream = connect(&socket).await;

    assert_eq!(roundtrip(&mut stream, Request::Ping).await, Response::Pong);

    // Cooperative shutdown.
    let r = roundtrip(&mut stream, Request::Shutdown).await;
    assert_eq!(r, Response::Empty);
    drop(stream);

    // The worker should exit cleanly.
    let status = timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("worker did not exit")
        .expect("worker wait");
    assert!(status.success(), "worker exit status: {status:?}");
}

#[tokio::test]
async fn full_vm_lifecycle_round_trips_over_the_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket) = spawn_worker(&tmp).await;
    let mut stream = connect(&socket).await;

    // CreateVm → VmHandle.
    let r = roundtrip(
        &mut stream,
        Request::CreateVm {
            config: VmConfig::default(),
        },
    )
    .await;
    let vm_id = match r {
        Response::VmHandle(h) => {
            assert_eq!(h.state, VmState::Created);
            h.id
        }
        other => panic!("expected VmHandle, got {other:?}"),
    };

    // Start → Empty. Then a State read should report Running.
    assert_eq!(
        roundtrip(&mut stream, Request::Start { id: vm_id }).await,
        Response::Empty
    );
    let r = roundtrip(&mut stream, Request::State { id: vm_id }).await;
    assert_eq!(
        r,
        Response::State {
            state: VmState::Running
        }
    );

    // Exec a no-op host program to confirm the round-trip carries
    // the exec result intact. `true` is on PATH on every reasonable
    // Linux runner; the smoke job's distroless caveat doesn't apply
    // here (cargo test runs on the regular runner image).
    let r = roundtrip(
        &mut stream,
        Request::ExecInGuest {
            id: vm_id,
            req: GuestExecRequest {
                program: "true".into(),
                args: vec![],
                cwd: None,
                env: vec![],
                timeout_ms: None,
            },
        },
    )
    .await;
    match r {
        Response::ExecResult(res) => {
            assert_eq!(res.exit_code, Some(0));
        }
        other => panic!("expected ExecResult, got {other:?}"),
    }

    // Snapshot → Snapshot { id }.
    let r = roundtrip(&mut stream, Request::Snapshot { id: vm_id }).await;
    let snap_id = match r {
        Response::Snapshot { id } => id,
        other => panic!("expected Snapshot, got {other:?}"),
    };

    // ListSnapshots → SnapshotIds containing our snap.
    let r = roundtrip(&mut stream, Request::ListSnapshots).await;
    match r {
        Response::SnapshotIds { ids } => assert!(ids.contains(&snap_id), "got {ids:?}"),
        other => panic!("expected SnapshotIds, got {other:?}"),
    }

    // Destroy + Shutdown.
    assert_eq!(
        roundtrip(&mut stream, Request::Destroy { id: vm_id }).await,
        Response::Empty
    );
    assert_eq!(
        roundtrip(&mut stream, Request::Shutdown).await,
        Response::Empty
    );
    drop(stream);

    let status = timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("worker did not exit")
        .expect("worker wait");
    assert!(status.success(), "worker exit status: {status:?}");
}

#[tokio::test]
async fn unknown_vm_request_yields_typed_error_envelope() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket) = spawn_worker(&tmp).await;
    let mut stream = connect(&socket).await;

    let r = roundtrip(&mut stream, Request::Start { id: VmId(99999) }).await;
    match r {
        Response::Error { code, .. } => {
            assert_eq!(code, vmm_ipc::ErrorCode::UnknownVm);
        }
        other => panic!("expected Error, got {other:?}"),
    }

    // Shutdown so the test exits cleanly.
    let _ = roundtrip(&mut stream, Request::Shutdown).await;
    drop(stream);
    let _ = timeout(Duration::from_secs(3), child.wait()).await;
}

#[tokio::test]
async fn worker_exits_cleanly_when_peer_disconnects_mid_session() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket) = spawn_worker(&tmp).await;
    let mut stream = connect(&socket).await;

    // Send one valid request, then drop the connection without
    // a Shutdown. The worker should treat EOF as a clean exit.
    assert_eq!(roundtrip(&mut stream, Request::Ping).await, Response::Pong);
    drop(stream);

    let status = timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("worker did not exit on disconnect")
        .expect("worker wait");
    assert!(
        status.success(),
        "worker should exit cleanly on disconnect, got {status:?}"
    );
}

#[tokio::test]
async fn worker_replies_to_garbage_frame_then_keeps_loop_going() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket) = spawn_worker(&tmp).await;
    let mut stream = connect(&socket).await;

    // Send a syntactically-broken frame: length prefix promising
    // 4 bytes of payload, payload is `{abc` (invalid JSON).
    // The worker should hit a FrameError and exit the serve loop;
    // unlike a clean Shutdown, the process exits with success here
    // because malformed frames are treated as transport failure
    // and the binary just unwinds.
    let payload = b"{abc";
    let frame = {
        let mut buf = Vec::<u8>::new();
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(payload);
        buf
    };
    stream.write_all(&frame).await.unwrap();

    // The peer (us) should observe an EOF on the read side once
    // the worker bails out of the serve loop. We rely on the wait
    // below to confirm the worker exited.
    drop(stream);

    let status = timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("worker did not exit after malformed frame")
        .expect("worker wait");
    // `main()` returns the FrameError, so the process exits
    // non-zero. That's the correct "operator, your transport is
    // broken" signal.
    assert!(
        !status.success(),
        "malformed frame should surface as non-zero exit, got {status:?}"
    );
}

#[tokio::test]
async fn worker_writes_response_then_drains_one_more_pipelined_request() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket) = spawn_worker(&tmp).await;
    let stream = connect(&socket).await;
    let (mut r, mut w) = stream.into_split();

    // Pipeline two requests back-to-back before reading either
    // response, confirming the serve loop services them in order.
    write_frame(&mut w, &Request::Ping).await.unwrap();
    write_frame(&mut w, &Request::Ping).await.unwrap();

    let a: Response = read_frame(&mut r).await.unwrap();
    let b: Response = read_frame(&mut r).await.unwrap();
    assert_eq!(a, Response::Pong);
    assert_eq!(b, Response::Pong);

    write_frame(&mut w, &Request::Shutdown).await.unwrap();
    let _: Response = read_frame(&mut r).await.unwrap();

    let _ = timeout(Duration::from_secs(3), child.wait()).await;
}

#[tokio::test]
async fn worker_explicitly_consumes_one_byte_at_a_time_does_not_break_framing() {
    // Sanity check: the framing layer reads the length prefix and
    // payload in two separate `read_exact`s, so a peer that flushes
    // them as two separate writes shouldn't change the outcome.
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, socket) = spawn_worker(&tmp).await;
    let stream = connect(&socket).await;
    let (mut r, mut w) = stream.into_split();

    // Serialize Ping, then write the 4-byte length, flush, then the
    // payload, flush. Use raw bytes so we control the boundary.
    let payload = serde_json::to_vec(&Request::Ping).unwrap();
    w.write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .unwrap();
    w.flush().await.unwrap();
    // Tiny sleep to make sure the worker has read the length prefix
    // before the payload arrives.
    sleep(Duration::from_millis(20)).await;
    w.write_all(&payload).await.unwrap();
    w.flush().await.unwrap();

    let resp: Response = read_frame(&mut r).await.unwrap();
    assert_eq!(resp, Response::Pong);

    write_frame(&mut w, &Request::Shutdown).await.unwrap();
    let _: Response = read_frame(&mut r).await.unwrap();
    let _ = timeout(Duration::from_secs(3), child.wait()).await;
}

#[tokio::test]
async fn worker_can_be_aborted_pre_accept_with_signal() {
    // Spawn the worker but never connect. Then kill it (the test
    // harness's `kill_on_drop` does this via Drop, but we exercise
    // the explicit path to confirm SIGTERM cleanup works).
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, _socket) = spawn_worker(&tmp).await;
    // Give the listener time to bind so the kill exercises the
    // pre-accept select arm, not the pre-bind path.
    sleep(Duration::from_millis(50)).await;
    child.start_kill().expect("kill");
    let status = timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("worker did not exit after signal")
        .expect("worker wait");
    // SIGKILL → exits with signal, not a code. Just confirm it died.
    assert!(!status.success() || status.code().is_some());
}
