//! Length-prefixed framing for `agent-sandbox-proto`.
//!
//! The vsock transport between host and guest is stream-oriented, so
//! both peers need a shared answer to "where does one JSON document
//! end and the next begin". This module is that answer:
//!
//! ```text
//! ┌────────────────────┬──────────────────────────┐
//! │ length (u32 LE)    │ JSON payload (length B)  │
//! └────────────────────┴──────────────────────────┘
//! ```
//!
//! - **4-byte little-endian** length prefix. LE matches the virtio
//!   convention (every `__le32` / `__le64` in
//!   `include/uapi/linux/virtio_vsock.h`) and is what the existing
//!   `guest-agent` framed-stdio mode emits. Consistent across host
//!   and guest, no byte-swap on either side.
//! - **No magic, no version byte** in the frame itself. Version is
//!   already part of every [`Request`] / [`Response`] payload via
//!   the `version` field; duplicating it framing-side just costs
//!   bytes and synchronization complexity.
//! - **Cap at [`MAX_FRAME_BYTES`]**. A peer (especially a malicious
//!   guest) could otherwise advertise a 4 GiB length and OOM the
//!   reader. 16 MiB is enough for any plausible single RPC payload
//!   (the biggest by far is `ReadFile`, capped at this size as a
//!   policy).
//!
//! The codec is sync + allocation-clear: callers pass a `&[u8]` or
//! `&mut Vec<u8>` and the helpers do the framing. We deliberately
//! don't tie this to tokio AsyncRead/Write — vsock might be sync
//! (`std::os::unix::net::UnixStream`-style ioctls) or async
//! (`tokio_vsock`), and the framing logic is identical.

use crate::{Request, Response};

/// Header size in bytes (the length prefix).
pub const HEADER_BYTES: usize = 4;

/// Maximum payload length we'll accept on a single frame. Beyond
/// this we reject the frame and the stream becomes unusable
/// (`InvalidLength`). Mirrors the upper bound on `ReadFile` content
/// and well above any realistic exec output chunk.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Framing errors. Anything other than [`FrameError::Incomplete`]
/// is a protocol-level failure — the stream should be closed.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// Reader has fewer bytes than required to parse a header or
    /// the announced payload. Caller should read more before trying
    /// again; this is *not* a protocol violation.
    #[error("frame incomplete: need {need} bytes, have {have}")]
    Incomplete {
        /// Bytes available in the buffer.
        have: usize,
        /// Bytes the frame needs to be parseable.
        need: usize,
    },
    /// Announced length exceeds [`MAX_FRAME_BYTES`].
    #[error("frame length {0} exceeds MAX_FRAME_BYTES ({MAX_FRAME_BYTES})")]
    InvalidLength(u32),
    /// JSON body failed to deserialize.
    #[error("frame payload failed to deserialize: {0}")]
    BadPayload(#[source] serde_json::Error),
    /// Encode side: payload serialized to more bytes than
    /// [`MAX_FRAME_BYTES`]. Programming error — the caller asked us
    /// to encode a giant Response that the peer would reject.
    #[error("encoded payload {bytes} bytes exceeds MAX_FRAME_BYTES ({MAX_FRAME_BYTES})")]
    PayloadTooLarge {
        /// Encoded size in bytes.
        bytes: usize,
    },
}

impl FrameError {
    /// `true` when the error is `Incomplete` — i.e. read more bytes.
    pub fn is_incomplete(&self) -> bool {
        matches!(self, FrameError::Incomplete { .. })
    }
}

/// Encode a [`Request`] as `len_prefix || json_payload` and append
/// to `out`. Returns the number of bytes appended (always
/// `HEADER_BYTES + payload.len()`).
pub fn encode_request(req: &Request, out: &mut Vec<u8>) -> Result<usize, FrameError> {
    encode_value(req, out)
}

/// Encode a [`Response`] as `len_prefix || json_payload` and append
/// to `out`. Returns the number of bytes appended.
pub fn encode_response(resp: &Response, out: &mut Vec<u8>) -> Result<usize, FrameError> {
    encode_value(resp, out)
}

fn encode_value<T: serde::Serialize>(v: &T, out: &mut Vec<u8>) -> Result<usize, FrameError> {
    // Reserve a slot for the header, write the payload, then patch
    // the header in place. This avoids serializing twice or
    // allocating an interim Vec.
    let header_start = out.len();
    out.extend_from_slice(&[0u8; HEADER_BYTES]);
    let payload_start = out.len();
    serde_json::to_writer(&mut *out, v).map_err(FrameError::BadPayload)?;
    let payload_len = out.len() - payload_start;
    if payload_len > MAX_FRAME_BYTES {
        // Truncate so caller's buffer doesn't carry a half-formed
        // frame around if they ignore the error.
        out.truncate(header_start);
        return Err(FrameError::PayloadTooLarge { bytes: payload_len });
    }
    let len_bytes = (payload_len as u32).to_le_bytes();
    out[header_start..header_start + HEADER_BYTES].copy_from_slice(&len_bytes);
    Ok(HEADER_BYTES + payload_len)
}

/// Try to decode one [`Request`] from `buf`. On success returns
/// `(Request, bytes_consumed)`; caller drops the consumed prefix
/// before calling again. On `Incomplete`, leave `buf` alone and
/// read more from the wire. Any other error is fatal for the
/// stream.
pub fn decode_request(buf: &[u8]) -> Result<(Request, usize), FrameError> {
    decode_value(buf)
}

/// Try to decode one [`Response`] from `buf`. Same semantics as
/// [`decode_request`].
pub fn decode_response(buf: &[u8]) -> Result<(Response, usize), FrameError> {
    decode_value(buf)
}

/// Parse the length prefix from a 4-byte header. Returns the
/// payload length the caller should read next, or
/// [`FrameError::InvalidLength`] if it exceeds [`MAX_FRAME_BYTES`].
///
/// Useful for stream-based readers (e.g. an `io::Read` loop) that
/// want to read the header first, allocate a payload buffer of the
/// right size, then read the body — rather than buffering the
/// whole stream and calling [`decode_request`] / [`decode_response`].
pub fn parse_len(header: &[u8; HEADER_BYTES]) -> Result<usize, FrameError> {
    let len = u32::from_le_bytes(*header);
    if (len as usize) > MAX_FRAME_BYTES {
        return Err(FrameError::InvalidLength(len));
    }
    Ok(len as usize)
}

/// Decode a [`Request`] from a payload whose length-prefix header
/// has already been stripped by the caller. Pairs with
/// [`parse_len`].
pub fn decode_request_payload(payload: &[u8]) -> Result<Request, FrameError> {
    serde_json::from_slice(payload).map_err(FrameError::BadPayload)
}

/// Decode a [`Response`] from a payload whose length-prefix header
/// has already been stripped by the caller. Pairs with
/// [`parse_len`].
pub fn decode_response_payload(payload: &[u8]) -> Result<Response, FrameError> {
    serde_json::from_slice(payload).map_err(FrameError::BadPayload)
}

fn decode_value<T: for<'de> serde::Deserialize<'de>>(buf: &[u8]) -> Result<(T, usize), FrameError> {
    if buf.len() < HEADER_BYTES {
        return Err(FrameError::Incomplete {
            have: buf.len(),
            need: HEADER_BYTES,
        });
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if (len as usize) > MAX_FRAME_BYTES {
        return Err(FrameError::InvalidLength(len));
    }
    let total = HEADER_BYTES + len as usize;
    if buf.len() < total {
        return Err(FrameError::Incomplete {
            have: buf.len(),
            need: total,
        });
    }
    let payload = &buf[HEADER_BYTES..total];
    let v = serde_json::from_slice::<T>(payload).map_err(FrameError::BadPayload)?;
    Ok((v, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrorCode, RequestBody, RequestId, ResponseBody, RpcError, PROTOCOL_VERSION};

    fn ping() -> Request {
        Request {
            version: PROTOCOL_VERSION,
            id: RequestId(1),
            body: RequestBody::Ping,
        }
    }

    #[test]
    fn encode_then_decode_request_roundtrips() {
        let mut buf = Vec::new();
        let n = encode_request(&ping(), &mut buf).unwrap();
        assert_eq!(n, buf.len());
        let (req, consumed) = decode_request(&buf).unwrap();
        assert_eq!(req, ping());
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn encode_then_decode_response_roundtrips() {
        let resp = Response {
            version: PROTOCOL_VERSION,
            id: RequestId(99),
            result: Ok(ResponseBody::Pong),
        };
        let mut buf = Vec::new();
        encode_response(&resp, &mut buf).unwrap();
        let (back, _) = decode_response(&buf).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn decode_with_only_partial_header_is_incomplete() {
        // 3 bytes — one short of HEADER_BYTES.
        let buf = vec![0u8, 0, 0];
        let err = decode_request(&buf).unwrap_err();
        assert!(err.is_incomplete(), "got {err:?}");
        assert!(matches!(err, FrameError::Incomplete { have: 3, need: 4 }));
    }

    #[test]
    fn decode_with_only_partial_payload_is_incomplete() {
        // Encode then truncate to header + 1 payload byte.
        let mut buf = Vec::new();
        encode_request(&ping(), &mut buf).unwrap();
        buf.truncate(HEADER_BYTES + 1);
        let err = decode_request(&buf).unwrap_err();
        assert!(err.is_incomplete());
    }

    #[test]
    fn two_back_to_back_frames_decode_independently() {
        let mut buf = Vec::new();
        encode_request(&ping(), &mut buf).unwrap();
        let mid = buf.len();
        encode_request(&ping(), &mut buf).unwrap();

        let (req1, n1) = decode_request(&buf).unwrap();
        assert_eq!(req1, ping());
        assert_eq!(n1, mid);

        let (req2, n2) = decode_request(&buf[n1..]).unwrap();
        assert_eq!(req2, ping());
        assert_eq!(n2, buf.len() - mid);
    }

    #[test]
    fn announced_length_over_cap_is_rejected_without_buffering() {
        // 4-byte BE length = MAX_FRAME_BYTES + 1, no payload.
        let bad_len = (MAX_FRAME_BYTES as u32 + 1).to_le_bytes();
        let err = decode_request(&bad_len).unwrap_err();
        assert!(matches!(err, FrameError::InvalidLength(_)));
    }

    #[test]
    fn malformed_payload_is_bad_payload_not_incomplete() {
        // 4-byte BE length = 5, payload = "junk!" → not a Request JSON.
        let mut buf = Vec::new();
        buf.extend_from_slice(&5u32.to_le_bytes());
        buf.extend_from_slice(b"junk!");
        let err = decode_request(&buf).unwrap_err();
        assert!(matches!(err, FrameError::BadPayload(_)), "got {err:?}");
    }

    #[test]
    fn encode_error_response_then_decode() {
        let resp = Response {
            version: PROTOCOL_VERSION,
            id: RequestId(13),
            result: Err(RpcError {
                code: ErrorCode::NoSuchProcess,
                message: "pid 9999".into(),
            }),
        };
        let mut buf = Vec::new();
        encode_response(&resp, &mut buf).unwrap();
        let (back, _) = decode_response(&buf).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn header_length_is_payload_length() {
        let mut buf = Vec::new();
        encode_request(&ping(), &mut buf).unwrap();
        let header = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert_eq!(header as usize, buf.len() - HEADER_BYTES);
    }

    #[test]
    fn empty_buffer_is_incomplete_with_zero_have() {
        let err = decode_request(&[]).unwrap_err();
        assert!(matches!(err, FrameError::Incomplete { have: 0, need: 4 }));
    }

    #[test]
    fn moderate_payload_roundtrips_and_oversize_payload_is_rejected() {
        // serde_json renders `Vec<u8>` as a JSON array of numbers
        // (~3-4 wire-bytes per source byte once commas, spaces and
        // multi-digit numbers are factored in), so the encode-side
        // cap kicks in well before the source `Vec` reaches
        // MAX_FRAME_BYTES.
        let ok = vec![0u8; 1024 * 1024]; // 1 MiB src → ~3 MiB on wire, well under cap
        let too_big = vec![0u8; 8 * 1024 * 1024]; // 8 MiB src → ~24 MiB on wire, over 16 MiB cap

        let mut buf = Vec::new();
        let resp_ok = Response {
            version: PROTOCOL_VERSION,
            id: RequestId(1),
            result: Ok(ResponseBody::FileContent { content: ok }),
        };
        encode_response(&resp_ok, &mut buf).expect("1 MiB payload must encode");
        let (back, _) = decode_response(&buf).expect("1 MiB payload must decode");
        assert_eq!(back.version, PROTOCOL_VERSION);

        let mut buf2 = Vec::new();
        let resp_big = Response {
            version: PROTOCOL_VERSION,
            id: RequestId(2),
            result: Ok(ResponseBody::FileContent { content: too_big }),
        };
        let err = encode_response(&resp_big, &mut buf2).unwrap_err();
        assert!(
            matches!(err, FrameError::PayloadTooLarge { .. }),
            "got {err:?}"
        );
        // On error the buffer must be left clean — caller can keep
        // using it for the next encode.
        assert!(
            buf2.is_empty(),
            "buffer was not cleaned up: {} bytes",
            buf2.len()
        );
    }

    #[test]
    fn encoded_header_is_little_endian() {
        // Pin the byte order against an external observer (the
        // existing guest-agent framed-stdio mode). Both sides MUST
        // agree on LE, so this is a wire-format contract test —
        // changing it is a protocol break.
        //
        // Construct a Request whose JSON serializes to a length
        // that's distinguishable BE vs LE. PROTOCOL_VERSION = 1 →
        // the smallest unambiguous example is the standard `Ping`,
        // which renders to a ~50-byte payload. Read the header
        // bytes and assert the first byte (LSB in LE) is the low
        // byte of the length and byte 3 (MSB in LE) is 0.
        let mut buf = Vec::new();
        encode_request(&ping(), &mut buf).unwrap();
        let payload_len = (buf.len() - HEADER_BYTES) as u32;
        assert!(
            payload_len > 0 && payload_len < 256,
            "expected a small payload, got {payload_len}"
        );
        assert_eq!(
            buf[0] as u32, payload_len,
            "byte 0 must be the LE low byte of the length"
        );
        assert_eq!(
            buf[1], 0,
            "byte 1 of LE length should be 0 for small payload"
        );
        assert_eq!(
            buf[2], 0,
            "byte 2 of LE length should be 0 for small payload"
        );
        assert_eq!(
            buf[3], 0,
            "byte 3 of LE length should be 0 for small payload"
        );
    }

    #[test]
    fn parse_len_round_trips_with_encode_header() {
        let mut buf = Vec::new();
        encode_request(&ping(), &mut buf).unwrap();
        let header: [u8; HEADER_BYTES] = buf[..HEADER_BYTES].try_into().unwrap();
        let payload_len = parse_len(&header).unwrap();
        assert_eq!(payload_len, buf.len() - HEADER_BYTES);
    }

    #[test]
    fn parse_len_rejects_oversize_announcement() {
        let oversize = (MAX_FRAME_BYTES as u32 + 1).to_le_bytes();
        let err = parse_len(&oversize).unwrap_err();
        assert!(matches!(err, FrameError::InvalidLength(_)), "got {err:?}");
    }

    #[test]
    fn decode_payload_helpers_pair_with_parse_len() {
        // Simulate a stream reader: read 4-byte header, then read
        // exactly `parse_len()` more bytes, then call decode_*_payload.
        let mut buf = Vec::new();
        let resp = Response {
            version: PROTOCOL_VERSION,
            id: RequestId(7),
            result: Ok(ResponseBody::Pong),
        };
        encode_response(&resp, &mut buf).unwrap();
        let header: [u8; HEADER_BYTES] = buf[..HEADER_BYTES].try_into().unwrap();
        let n = parse_len(&header).unwrap();
        let payload = &buf[HEADER_BYTES..HEADER_BYTES + n];
        let back = decode_response_payload(payload).unwrap();
        assert_eq!(back, resp);
    }
}
