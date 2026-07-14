//! The per-window render thread (spec §7).
//!
//! A dedicated std thread owns the UDS connection and a `!Send` [`GhosttyVt`]
//! (kept off the iced runtime, which is multi-threaded). It feeds the daemon's
//! Snapshot/Output into the terminal, produces [`RenderSnapshot`]s out one
//! channel, and takes key/resize commands in another — `encode_key` uses the
//! render terminal's own mode state, so DECCKM/kitty stay in sync for free.

use std::path::PathBuf;

use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, PROTO_VERSION, code};
use asd_vt::{GhosttyVt, KeyEvent, RenderSnapshot, VtBackend};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Scrollback kept by the render terminal (viewport scrolling is M1+ in the
/// GUI; the buffer is here so history survives resize/reflow).
const SCROLLBACK: usize = 10_000;

/// Commands the app sends to the render thread.
#[derive(Debug, Clone)]
pub enum Cmd {
    /// A key press to encode and forward as Input.
    Key(KeyEvent),
    /// The window was resized.
    Resize { cols: u16, rows: u16 },
}

/// Events the render thread sends to the app.
#[derive(Debug, Clone)]
pub enum WorkerEvent {
    /// A fresh frame to draw.
    Frame(Box<RenderSnapshot>),
    /// The session's program exited (with a human-readable reason).
    SessionEnded(String),
    /// The connection dropped or failed (with a reason).
    Disconnected(String),
}

/// Run the connection to completion on the calling thread. Intended to be
/// `std::thread::spawn`ed; it builds its own current-thread tokio runtime so
/// the `!Send` terminal never crosses threads.
pub fn run(
    socket: PathBuf,
    name: String,
    cols: u16,
    rows: u16,
    cmd_rx: UnboundedReceiver<Cmd>,
    ev_tx: UnboundedSender<WorkerEvent>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ev_tx.send(WorkerEvent::Disconnected(format!("runtime: {e}")));
            return;
        }
    };
    let reason = runtime.block_on(drive(socket, name, cols, rows, cmd_rx, &ev_tx));
    let _ = ev_tx.send(reason);
}

/// Returns the terminal `WorkerEvent` describing how the connection ended.
async fn drive(
    socket: PathBuf,
    name: String,
    cols: u16,
    rows: u16,
    mut cmd_rx: UnboundedReceiver<Cmd>,
    ev_tx: &UnboundedSender<WorkerEvent>,
) -> WorkerEvent {
    let stream = match UnixStream::connect(&socket).await {
        Ok(s) => s,
        Err(e) => {
            return WorkerEvent::Disconnected(format!("connect {}: {e}", socket.display()));
        }
    };
    let (r, w) = stream.into_split();
    let mut reader = FrameReader::new(r);
    let mut writer = FrameWriter::new(w);

    // Handshake.
    if writer
        .write_frame(&Frame::Hello {
            proto_version: PROTO_VERSION,
            kind: ClientKind::Gui,
        })
        .await
        .is_err()
    {
        return WorkerEvent::Disconnected("handshake write failed".into());
    }
    match reader.read_frame().await {
        Ok(Some(Frame::HelloAck { .. })) => {}
        Ok(Some(Frame::Error { code, msg })) => {
            return WorkerEvent::Disconnected(format!("handshake rejected ({code}): {msg}"));
        }
        _ => return WorkerEvent::Disconnected("no handshake ack".into()),
    }

    // Attach and read the first Snapshot.
    if writer
        .write_frame(&Frame::Attach {
            name: name.clone(),
            cols,
            rows,
        })
        .await
        .is_err()
    {
        return WorkerEvent::Disconnected("attach write failed".into());
    }
    let first = match reader.read_frame().await {
        Ok(Some(Frame::Snapshot { vt })) => vt,
        Ok(Some(Frame::Error { code, msg })) => {
            return WorkerEvent::Disconnected(format!("attach failed ({code}): {msg}"));
        }
        _ => return WorkerEvent::Disconnected("no snapshot after attach".into()),
    };

    let mut vt = GhosttyVt::new(cols.max(1), rows.max(1), SCROLLBACK);
    vt.feed(&first);
    let _ = vt.take_pty_responses(); // the daemon already answered any queries
    if ev_tx
        .send(WorkerEvent::Frame(Box::new(vt.render_snapshot())))
        .is_err()
    {
        return WorkerEvent::Disconnected("app gone".into());
    }

    loop {
        tokio::select! {
            frame = reader.read_frame() => match frame {
                Ok(Some(Frame::Output { bytes })) => {
                    vt.feed(&bytes);
                    let _ = vt.take_pty_responses();
                    if ev_tx.send(WorkerEvent::Frame(Box::new(vt.render_snapshot()))).is_err() {
                        return WorkerEvent::Disconnected("app gone".into());
                    }
                }
                Ok(Some(Frame::Snapshot { vt: dump })) => {
                    vt.feed(&dump);
                    let _ = vt.take_pty_responses();
                    if ev_tx.send(WorkerEvent::Frame(Box::new(vt.render_snapshot()))).is_err() {
                        return WorkerEvent::Disconnected("app gone".into());
                    }
                }
                Ok(Some(Frame::Error { code, msg })) => {
                    return if code == code::SESSION_EXITED {
                        WorkerEvent::SessionEnded(msg)
                    } else {
                        WorkerEvent::Disconnected(format!("error {code}: {msg}"))
                    };
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return WorkerEvent::Disconnected("connection closed".into()),
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::Key(ev)) => {
                    let bytes = vt.encode_key(ev);
                    if !bytes.is_empty()
                        && writer.write_frame(&Frame::Input { bytes }).await.is_err()
                    {
                        return WorkerEvent::Disconnected("input write failed".into());
                    }
                }
                Some(Cmd::Resize { cols, rows }) => {
                    vt.resize(cols.max(1), rows.max(1));
                    if writer.write_frame(&Frame::Resize { cols, rows }).await.is_err() {
                        return WorkerEvent::Disconnected("resize write failed".into());
                    }
                    if ev_tx.send(WorkerEvent::Frame(Box::new(vt.render_snapshot()))).is_err() {
                        return WorkerEvent::Disconnected("app gone".into());
                    }
                }
                None => {
                    // The app dropped the command channel (window closed):
                    // detach cleanly, then end.
                    let _ = writer.write_frame(&Frame::Detach).await;
                    return WorkerEvent::Disconnected("closed".into());
                }
            },
        }
    }
}
