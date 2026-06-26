//! End-to-end protocol test: drive [`Request`] and [`Response`] over
//! an `tokio::io::duplex` channel (the in-memory analogue of a Unix
//! socket pair) and verify the framing + serde layer composes.

use tokio::io::duplex;
use vm_core::{GuestExecRequest, GuestExecResult, SnapshotId, VmConfig, VmHandle, VmId, VmState};
use vmm_ipc::framing::{read_frame, write_frame};
use vmm_ipc::{ErrorCode, Request, Response};

#[tokio::test]
async fn full_lifecycle_sequence_round_trips() {
    let (mut host, mut child) = duplex(64 * 1024);

    // Host writes a sequence of requests.
    let requests = vec![
        Request::Ping,
        Request::CreateVm {
            config: VmConfig::default(),
        },
        Request::Start { id: VmId(1) },
        Request::ExecInGuest {
            id: VmId(1),
            req: GuestExecRequest {
                program: "true".into(),
                args: vec![],
                cwd: None,
                env: vec![],
                timeout_ms: None,
            },
        },
        Request::Snapshot { id: VmId(1) },
        Request::Stop { id: VmId(1) },
        Request::Destroy { id: VmId(1) },
        Request::Shutdown,
    ];

    // Drive both sides on the same task. With the duplex set to
    // 64 KiB there's headroom for the whole sequence before the
    // reader catches up, so we don't need a separate task or
    // back-pressure handling for the test.
    for req in &requests {
        write_frame(&mut host, req).await.unwrap();
    }

    for expected in &requests {
        let got: Request = read_frame(&mut child).await.unwrap();
        assert_eq!(got, *expected);
    }
}

#[tokio::test]
async fn responses_round_trip_each_variant() {
    let (mut a, mut b) = duplex(64 * 1024);

    let exec_result = GuestExecResult {
        exit_code: Some(0),
        signal: None,
        stdout: b"hi\n".to_vec(),
        stderr: vec![],
        duration_ms: 12,
    };

    let cases = vec![
        Response::Pong,
        Response::Empty,
        Response::VmHandle(VmHandle {
            id: VmId(7),
            state: VmState::Running,
        }),
        Response::State {
            state: VmState::Stopped,
        },
        Response::Snapshot { id: SnapshotId(3) },
        Response::ExecResult(exec_result),
        Response::Bytes {
            content: vec![1, 2, 3, 4],
        },
        Response::Written { bytes: 17 },
        Response::error(ErrorCode::UnknownVm, "no such vm"),
    ];

    for r in &cases {
        write_frame(&mut a, r).await.unwrap();
    }
    for expected in &cases {
        let got: Response = read_frame(&mut b).await.unwrap();
        assert_eq!(got, *expected);
    }
}

#[tokio::test]
async fn host_to_child_and_back_simulates_one_op() {
    // Two duplex pairs: one "host → child", one "child → host".
    // Models the Unix-socket bidirection that the real worker will
    // use (one stream is read-half, the other is write-half from
    // each side's perspective).
    let (mut h_to_c, mut c_in) = duplex(8192);
    let (mut c_out, mut h_from_c) = duplex(8192);

    // Host sends a Ping.
    write_frame(&mut h_to_c, &Request::Ping).await.unwrap();
    // Child receives it, replies Pong.
    let req: Request = read_frame(&mut c_in).await.unwrap();
    assert_eq!(req, Request::Ping);
    write_frame(&mut c_out, &Response::Pong).await.unwrap();
    // Host reads the reply.
    let resp: Response = read_frame(&mut h_from_c).await.unwrap();
    assert_eq!(resp, Response::Pong);
}
