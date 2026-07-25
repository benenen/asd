//! Client-side logic shared by every asd client (asd-cli, asd-tui,
//! asd-dioxus).
//!
//! The wire format itself — frames, codec, framed reader/writer, path
//! contract — lives in `asd-proto`, which the daemon shares. What lives here
//! is the half only a *client* runs: opening a connection ([`handshake`]) and
//! deciding which arriving frame belongs to the view currently on screen
//! ([`attach::Attach`]). Keeping it out of `asd-proto` means the daemon does
//! not compile client bookkeeping it never executes, and the three clients
//! cannot drift apart on rules they must all observe identically.

pub mod attach;

use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, PROTO_VERSION};
use tokio::io::{AsyncRead, AsyncWrite};

/// Run the client side of the asd handshake: send Hello, expect HelloAck.
/// Returns the daemon's version string on success.
///
/// This is the one handshake shared by every asd client (CLI, TUI, GUI); a
/// version mismatch or bad reply is returned as an error.
pub async fn handshake(
    writer: &mut FrameWriter<impl AsyncWrite + Unpin>,
    reader: &mut FrameReader<impl AsyncRead + Unpin>,
    kind: ClientKind,
) -> Result<String, String> {
    writer
        .write_frame(&Frame::Hello {
            proto_version: PROTO_VERSION,
            kind,
        })
        .await
        .map_err(|_| "handshake write failed".to_string())?;
    match reader.read_frame().await {
        Ok(Some(Frame::HelloAck { daemon_version, .. })) => Ok(daemon_version),
        Ok(Some(Frame::Error { code, msg })) => {
            Err(format!("daemon rejected handshake ({code}): {msg}"))
        }
        Ok(_) => Err("unexpected handshake reply".to_string()),
        Err(e) => Err(format!("handshake read failed: {e}")),
    }
}
