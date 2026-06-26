//! Length-prefixed frame I/O for [`Request`] and [`Response`].
//!
//! Wire shape: `[len: u32 big-endian][payload: <len> bytes]`. The
//! payload is UTF-8 JSON, never null-terminated. The reader rejects
//! frames larger than [`DEFAULT_MAX_FRAME_BYTES`] (overridable per
//! call) so a confused or hostile peer can't make us allocate the
//! whole heap.
//!
//! [`Request`]: crate::Request
//! [`Response`]: crate::Response

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::DEFAULT_MAX_FRAME_BYTES;

/// Framing-layer errors. Distinct from semantic errors (which travel
/// as [`crate::Response::Error`] inside a successful frame): hitting a
/// `FrameError` means the wire itself is broken or the peer went
/// away, not that an op failed.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The underlying transport returned an I/O error
    /// (connection reset, broken pipe, etc.).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The peer announced a frame larger than the configured cap.
    /// Treated as adversarial — we don't allocate and don't read.
    #[error("frame too large: announced {announced} bytes, cap {cap}")]
    FrameTooLarge {
        /// Bytes the peer announced in the length prefix.
        announced: usize,
        /// Configured cap for this reader.
        cap: usize,
    },
    /// JSON deserialization failed. Indicates a peer protocol bug
    /// (mismatched crate versions, hand-rolled message that doesn't
    /// match the schema, etc.).
    #[error("malformed JSON payload: {0}")]
    BadJson(#[from] serde_json::Error),
}

/// Write a single frame to `w`: 4-byte big-endian length, then the
/// JSON-encoded payload. The frame is flushed before returning so a
/// caller that yields immediately after still gets ordering
/// guarantees.
pub async fn write_frame<W, T>(w: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let payload = serde_json::to_vec(value)?;
    // u32::MAX caps the wire-format frame size at ~4 GiB; in practice
    // we'll hit DEFAULT_MAX_FRAME_BYTES at 4 MiB long before that, but
    // be defensive in case a caller turns the cap up.
    let len = u32::try_from(payload.len()).map_err(|_| FrameError::FrameTooLarge {
        announced: payload.len(),
        cap: u32::MAX as usize,
    })?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&payload).await?;
    w.flush().await?;
    Ok(())
}

/// Read one frame from `r` using the default cap.
pub async fn read_frame<R, T>(r: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    read_frame_with_cap(r, DEFAULT_MAX_FRAME_BYTES).await
}

/// Read one frame from `r`, rejecting any frame whose announced
/// length exceeds `cap`.
pub async fn read_frame_with_cap<R, T>(r: &mut R, cap: usize) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let announced = u32::from_be_bytes(len_buf) as usize;
    if announced > cap {
        return Err(FrameError::FrameTooLarge { announced, cap });
    }
    let mut payload = vec![0u8; announced];
    r.read_exact(&mut payload).await?;
    let value = serde_json::from_slice(&payload)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Request, Response};
    use vm_core::{VmHandle, VmId, VmState};

    #[tokio::test]
    async fn frame_round_trips_a_request() {
        let req = Request::Start { id: VmId(99) };
        let mut buf = Vec::<u8>::new();
        write_frame(&mut buf, &req).await.unwrap();

        // Length prefix is the first 4 bytes, big-endian.
        let len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
        assert_eq!(len, buf.len() - 4);

        let mut cursor = std::io::Cursor::new(buf);
        let back: Request = read_frame(&mut cursor).await.unwrap();
        assert_eq!(back, req);
    }

    #[tokio::test]
    async fn frame_round_trips_a_response() {
        let resp = Response::VmHandle(VmHandle {
            id: VmId(1),
            state: VmState::Running,
        });
        let mut buf = Vec::<u8>::new();
        write_frame(&mut buf, &resp).await.unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let back: Response = read_frame(&mut cursor).await.unwrap();
        assert_eq!(back, resp);
    }

    #[tokio::test]
    async fn back_to_back_frames_decode_in_order() {
        let mut buf = Vec::<u8>::new();
        write_frame(&mut buf, &Request::Ping).await.unwrap();
        write_frame(&mut buf, &Request::Shutdown).await.unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let a: Request = read_frame(&mut cursor).await.unwrap();
        let b: Request = read_frame(&mut cursor).await.unwrap();
        assert_eq!(a, Request::Ping);
        assert_eq!(b, Request::Shutdown);
    }

    #[tokio::test]
    async fn oversize_frame_is_rejected_without_allocating() {
        // Craft a length-prefix that lies: claim 8 MiB, cap at 1 KiB.
        let mut buf = Vec::<u8>::new();
        buf.extend_from_slice(&(8u32 * 1024 * 1024).to_be_bytes());
        // Note: we don't include payload bytes — the reader should
        // refuse to allocate before it ever reads them.
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame_with_cap::<_, Request>(&mut cursor, 1024)
            .await
            .unwrap_err();
        assert!(matches!(err, FrameError::FrameTooLarge { .. }));
    }

    #[tokio::test]
    async fn truncated_payload_surfaces_as_io_error() {
        // Announce 100 bytes, supply 10.
        let mut buf = Vec::<u8>::new();
        buf.extend_from_slice(&100u32.to_be_bytes());
        buf.extend_from_slice(&[0u8; 10]);
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame::<_, Request>(&mut cursor).await.unwrap_err();
        assert!(matches!(err, FrameError::Io(_)));
    }

    #[tokio::test]
    async fn malformed_json_payload_surfaces_as_bad_json() {
        // Length prefix says 4 bytes, payload is the literal `{abc`
        // which `serde_json` will reject.
        let payload = b"{abc";
        let mut buf = Vec::<u8>::new();
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(payload);
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame::<_, Request>(&mut cursor).await.unwrap_err();
        assert!(matches!(err, FrameError::BadJson(_)));
    }
}
