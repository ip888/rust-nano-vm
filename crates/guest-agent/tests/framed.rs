//! End-to-end test for the framed-stdio mode of `nanovm-agent`.
//!
//! Spawns the compiled binary with `NANOVM_AGENT_FRAMED=1`, pipes
//! encoded `proto::Request` frames into its stdin, reads framed
//! `proto::Response` bytes from its stdout, and decodes them.
//! Validates the whole framing path host-side — the same path the
//! virtio-vsock device will drive once M2 wires the transport in.
//!
//! We don't link the agent's `main` binary directly because that
//! would require exposing handler internals from a `[[bin]]`
//! target. Spawning is cheaper and exercises the real entrypoint,
//! including the env-var dispatch in `main`.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use proto::{
    decode_response_payload, encode_request, parse_len, Request, RequestBody, RequestId, Response,
    ResponseBody, HEADER_BYTES, PROTOCOL_VERSION,
};

/// Path to the just-built `nanovm-agent` binary. Cargo's
/// `CARGO_BIN_EXE_<name>` env var is set for `tests/` integration
/// tests and points at the freshly-built artifact.
fn agent_bin() -> &'static str {
    env!("CARGO_BIN_EXE_nanovm-agent")
}

/// Encode `reqs` as framed bytes, pipe into the agent on stdin,
/// read framed responses off stdout, decode them all, and return.
/// Asserts the agent exits cleanly when stdin closes.
fn round_trip(reqs: Vec<Request>) -> Vec<Response> {
    let mut input_bytes = Vec::new();
    for req in &reqs {
        encode_request(req, &mut input_bytes).expect("encode request");
    }

    let mut child = Command::new(agent_bin())
        .env("NANOVM_AGENT_FRAMED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn nanovm-agent");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        stdin.write_all(&input_bytes).expect("write stdin");
        stdin.flush().expect("flush stdin");
    }
    // Close stdin so the agent's request loop sees EOF and exits.
    drop(child.stdin.take());

    let mut stdout = Vec::new();
    child
        .stdout
        .as_mut()
        .expect("stdout")
        .read_to_end(&mut stdout)
        .expect("read stdout");

    let status = child.wait().expect("wait for agent");
    if !status.success() {
        let mut err = String::new();
        if let Some(mut s) = child.stderr {
            let _ = s.read_to_string(&mut err);
        }
        panic!("agent exited with {status:?}: {err}");
    }

    // Decode all responses out of stdout.
    let mut out = Vec::new();
    let mut cursor = 0;
    while cursor + HEADER_BYTES <= stdout.len() {
        let header: [u8; HEADER_BYTES] = stdout[cursor..cursor + HEADER_BYTES]
            .try_into()
            .expect("4 bytes");
        let payload_len = parse_len(&header).expect("parse len");
        cursor += HEADER_BYTES;
        let payload = &stdout[cursor..cursor + payload_len];
        out.push(decode_response_payload(payload).expect("decode payload"));
        cursor += payload_len;
    }
    assert_eq!(
        cursor,
        stdout.len(),
        "trailing {} bytes after last frame",
        stdout.len() - cursor
    );
    out
}

#[test]
fn ping_returns_pong() {
    let resps = round_trip(vec![Request {
        version: PROTOCOL_VERSION,
        id: RequestId(1),
        body: RequestBody::Ping,
    }]);
    assert_eq!(resps.len(), 1);
    assert_eq!(resps[0].version, PROTOCOL_VERSION);
    assert_eq!(resps[0].id, RequestId(1));
    assert!(matches!(resps[0].result, Ok(ResponseBody::Pong)));
}

#[test]
fn three_pings_each_get_their_id_back() {
    // Pipelined: send three requests in one shot, expect three
    // responses correlated by id.
    let resps = round_trip(
        (10..13)
            .map(|i| Request {
                version: PROTOCOL_VERSION,
                id: RequestId(i),
                body: RequestBody::Ping,
            })
            .collect(),
    );
    assert_eq!(resps.len(), 3);
    for (i, resp) in resps.iter().enumerate() {
        assert_eq!(resp.id, RequestId(10 + i as u64));
        assert!(matches!(resp.result, Ok(ResponseBody::Pong)));
    }
}

#[test]
fn exec_echo_captures_stdout_through_framed_path() {
    let resps = round_trip(vec![Request {
        version: PROTOCOL_VERSION,
        id: RequestId(42),
        body: RequestBody::Exec {
            program: "echo".into(),
            args: vec!["hello-framed".into()],
            cwd: None,
            env: vec![],
            timeout_ms: None,
        },
    }]);
    assert_eq!(resps.len(), 1);
    assert_eq!(resps[0].id, RequestId(42));
    match &resps[0].result {
        Ok(ResponseBody::ExecResult {
            exit_code, stdout, ..
        }) => {
            assert_eq!(*exit_code, Some(0));
            assert!(stdout.starts_with(b"hello-framed"), "stdout was {stdout:?}");
        }
        other => panic!("expected ExecResult, got {other:?}"),
    }
}

#[test]
fn malformed_payload_yields_bad_request_response_with_id_zero() {
    // Hand-craft a frame with a valid length prefix but a JSON
    // body that isn't a Request. The agent should send back a
    // BadRequest with id 0 (since it couldn't even parse the id)
    // and keep the connection alive for the next request.
    let mut input = Vec::new();
    let bad = b"\"not a request envelope\"";
    input.extend_from_slice(&(bad.len() as u32).to_le_bytes());
    input.extend_from_slice(bad);
    // Follow up with a valid Ping so we also verify the loop
    // didn't terminate on the malformed frame.
    encode_request(
        &Request {
            version: PROTOCOL_VERSION,
            id: RequestId(99),
            body: RequestBody::Ping,
        },
        &mut input,
    )
    .unwrap();

    let mut child = Command::new(agent_bin())
        .env("NANOVM_AGENT_FRAMED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(&input).unwrap();
    drop(child.stdin.take());

    let mut stdout = Vec::new();
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    let _ = child.wait();

    // Decode all responses.
    let mut resps = Vec::new();
    let mut cursor = 0;
    while cursor + HEADER_BYTES <= stdout.len() {
        let header: [u8; HEADER_BYTES] = stdout[cursor..cursor + HEADER_BYTES].try_into().unwrap();
        let n = parse_len(&header).unwrap();
        cursor += HEADER_BYTES;
        resps.push(decode_response_payload(&stdout[cursor..cursor + n]).unwrap());
        cursor += n;
    }

    assert_eq!(resps.len(), 2, "got {} response(s)", resps.len());
    // First reply: BadRequest with id 0.
    assert_eq!(resps[0].id, RequestId(0));
    assert!(
        matches!(
            resps[0].result,
            Err(proto::RpcError {
                code: proto::ErrorCode::BadRequest,
                ..
            })
        ),
        "got {:?}",
        resps[0].result
    );
    // Second reply: the loop survived; pong for id 99.
    assert_eq!(resps[1].id, RequestId(99));
    assert!(matches!(resps[1].result, Ok(ResponseBody::Pong)));
}
