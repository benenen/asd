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
///
/// The half-read frame lives here rather than in [`FrameReader::read_frame`]'s
/// future, which is what makes that future safe to cancel — see its docs.
#[derive(Debug)]
pub struct FrameReader<R> {
    inner: R,
    /// The length prefix, filled across as many reads as it takes.
    len_buf: [u8; 4],
    len_filled: usize,
    /// The payload buffer, sized once the prefix is complete, then filled the
    /// same way. `None` while the prefix is still arriving.
    payload: Option<Vec<u8>>,
    payload_filled: usize,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            len_buf: [0u8; 4],
            len_filled: 0,
            payload: None,
            payload_filled: 0,
        }
    }

    /// Take the transport back. Any partially read frame is discarded with the
    /// reader, so only do this at a frame boundary.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Read the next frame.
    ///
    /// - EOF at a frame boundary returns `Ok(None)` (peer closed cleanly);
    /// - EOF mid-frame returns `Err(Io(UnexpectedEof))`;
    /// - a length prefix over 4 MiB returns [`ProtoError::FrameTooLarge`];
    ///   the caller should disconnect.
    ///
    /// Cancellation-safe: every byte taken from the transport is recorded in
    /// the reader before the next await, so dropping this future loses nothing
    /// and the next call resumes the same frame. Clients drive their connection
    /// with `tokio::select!`, which drops this future every time a heartbeat or
    /// a keystroke wins the race; a future that owned the partial frame would
    /// take those bytes with it and leave the stream resuming mid-payload —
    /// read as a length prefix, that ends the connection on a nonsense length.
    pub async fn read_frame(&mut self) -> Result<Option<Frame>, ProtoError> {
        // The length prefix, read with a manual loop: EOF at 0 bytes is a clean
        // close, EOF partway through is a truncation error (read_exact cannot
        // distinguish the two).
        while self.len_filled < self.len_buf.len() {
            let n = self
                .inner
                .read(&mut self.len_buf[self.len_filled..])
                .await?;
            if n == 0 {
                if self.len_filled == 0 {
                    return Ok(None);
                }
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            self.len_filled += n;
        }

        // Size the payload the first time the prefix completes; a resumed read
        // finds it already there.
        if self.payload.is_none() {
            let len = u32::from_le_bytes(self.len_buf) as usize;
            if len > MAX_FRAME_LEN {
                return Err(ProtoError::FrameTooLarge(len));
            }
            self.payload = Some(vec![0u8; len]);
            self.payload_filled = 0;
        }
        let payload = self.payload.as_mut().expect("payload was just sized");
        while self.payload_filled < payload.len() {
            let n = self.inner.read(&mut payload[self.payload_filled..]).await?;
            if n == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            self.payload_filled += n;
        }

        // Whole frame in hand: clear the state before decoding, so a frame the
        // codec rejects does not leave its bytes behind for the next call.
        let payload = self.payload.take().expect("payload was just filled");
        self.len_filled = 0;
        self.payload_filled = 0;
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
