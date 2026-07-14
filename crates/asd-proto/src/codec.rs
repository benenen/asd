//! Frame encoding/decoding and the framed reader/writer.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{Frame, MAX_FRAME_LEN, ProtoError};

/// Encode a frame as a `u32 LE length prefix + postcard` byte string.
///
/// Refuses to encode payloads exceeding [`MAX_FRAME_LEN`] — the sending side
/// is bound by the same 4 MiB contract.
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, ProtoError> {
    let payload = postcard::to_stdvec(frame)?;
    if payload.len() > MAX_FRAME_LEN {
        return Err(ProtoError::FrameTooLarge(payload.len()));
    }
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Decode a frame from a complete postcard payload (without the length prefix).
pub fn decode_frame(payload: &[u8]) -> Result<Frame, ProtoError> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(ProtoError::FrameTooLarge(payload.len()));
    }
    Ok(postcard::from_bytes(payload)?)
}

/// Frame reading end over any `AsyncRead`.
#[derive(Debug)]
pub struct FrameReader<R> {
    inner: R,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Read the next frame.
    ///
    /// - EOF at a frame boundary returns `Ok(None)` (peer closed cleanly);
    /// - EOF mid-frame returns `Err(Io(UnexpectedEof))`;
    /// - a length prefix over 4 MiB returns [`ProtoError::FrameTooLarge`];
    ///   the caller should disconnect.
    pub async fn read_frame(&mut self) -> Result<Option<Frame>, ProtoError> {
        // Read the length prefix with a manual loop: EOF at 0 bytes is a clean
        // close, EOF partway through is a truncation error (read_exact cannot
        // distinguish the two).
        let mut len_buf = [0u8; 4];
        let mut filled = 0;
        while filled < len_buf.len() {
            let n = self.inner.read(&mut len_buf[filled..]).await?;
            if n == 0 {
                if filled == 0 {
                    return Ok(None);
                }
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            filled += n;
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > MAX_FRAME_LEN {
            return Err(ProtoError::FrameTooLarge(len));
        }
        let mut payload = vec![0u8; len];
        self.inner.read_exact(&mut payload).await?;
        Ok(Some(decode_frame(&payload)?))
    }
}

/// Frame writing end over any `AsyncWrite`.
#[derive(Debug)]
pub struct FrameWriter<W> {
    inner: W,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    pub fn into_inner(self) -> W {
        self.inner
    }

    /// Encode and write out one frame, then flush.
    pub async fn write_frame(&mut self, frame: &Frame) -> Result<(), ProtoError> {
        let buf = encode_frame(frame)?;
        self.inner.write_all(&buf).await?;
        self.inner.flush().await?;
        Ok(())
    }
}
