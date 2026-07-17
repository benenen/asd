//! Daemon connection on a background thread (mirrors asd-dioxus's host actor,
//! local-only): the TUI thread owns the `!Send` terminal, so only plain data
//! crosses the two std channels here.
//!
//! The actor handshakes, polls `ListSessions` for the sidebar, and while
//! attached forwards raw Snapshot/Output bytes tagged with the session they
//! belong to. The `pending_attach` counter drops frames of superseded attaches
//! so a quick session switch can't paint stale content (same race as the GUI
//! clients).

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, PROTO_VERSION, code};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// How often the session list is re-polled.
const LIST_INTERVAL: Duration = Duration::from_millis(1500);

/// Commands the TUI sends to the connection actor.
#[derive(Debug, Clone)]
pub enum Cmd {
    /// Attach to (or switch to) `name`, sizing the pty to `cols`×`rows`.
    Attach {
        name: String,
        cols: u16,
        rows: u16,
    },
    /// Raw input bytes for the attached session.
    Input(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Create a new session (daemon auto-names it).
    Create,
    Kill {
        name: String,
    },
    /// Disconnect and end the actor.
    Shutdown,
}

/// Events the actor sends toward the TUI thread.
#[derive(Debug)]
pub enum Ev {
    Up,
    Down(String),
    Sessions(Vec<asd_proto::SessionInfo>),
    /// A `Create` completed; the TUI selects `name`.
    Created(String),
    /// PTY bytes for the session named `name`; `snapshot` marks the full
    /// attach dump (the TUI resets its terminal before feeding it).
    Bytes {
        name: String,
        data: Vec<u8>,
        snapshot: bool,
    },
    SessionEnded {
        name: String,
        msg: String,
    },
}

/// Handle to the running actor thread.
pub struct Conn {
    pub cmd_tx: UnboundedSender<Cmd>,
}

impl Conn {
    /// Spawn the actor thread with its own current-thread runtime. Events —
    /// including connect/handshake failures — arrive on `ev_tx`.
    pub fn spawn(socket: PathBuf, ev_tx: Sender<Ev>) -> Self {
        let (cmd_tx, cmd_rx) = unbounded_channel::<Cmd>();
        std::thread::Builder::new()
            .name("asd-tui-conn".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ev_tx.send(Ev::Down(format!("runtime: {e}")));
                        return;
                    }
                };
                rt.block_on(async move {
                    if let Err(reason) = drive(&socket, cmd_rx, &ev_tx).await {
                        let _ = ev_tx.send(Ev::Down(reason));
                    }
                });
            })
            .expect("conn thread");
        Self { cmd_tx }
    }
}

/// The connection event loop. Returns `Err(reason)` if the connection ends
/// abnormally; a clean `Shutdown` returns `Ok(())`.
async fn drive(
    socket: &PathBuf,
    mut cmd_rx: UnboundedReceiver<Cmd>,
    ev_tx: &Sender<Ev>,
) -> Result<(), String> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let (r, w) = stream.into_split();
    let mut reader = FrameReader::new(r);
    let mut writer = FrameWriter::new(w);

    writer
        .write_frame(&Frame::Hello {
            proto_version: PROTO_VERSION,
            kind: ClientKind::Cli,
        })
        .await
        .map_err(|_| "handshake write failed".to_string())?;
    match reader.read_frame().await {
        Ok(Some(Frame::HelloAck { .. })) => {}
        Ok(Some(Frame::Error { code, msg })) => {
            return Err(format!("handshake rejected ({code}): {msg}"));
        }
        _ => return Err("no handshake ack".to_string()),
    }
    let _ = ev_tx.send(Ev::Up);

    // Attach frames sent whose Snapshot has not arrived yet: while > 0 Output
    // is stale, while > 1 arriving Snapshots belong to superseded attaches.
    let mut pending_attach: usize = 0;
    let mut attached: Option<String> = None;

    let mut ticker = tokio::time::interval(LIST_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if writer.write_frame(&Frame::ListSessions).await.is_err() {
                    return Err("list write failed".to_string());
                }
            }
            frame = reader.read_frame() => match frame {
                Ok(Some(Frame::SessionList { sessions })) => {
                    let _ = ev_tx.send(Ev::Sessions(sessions));
                }
                Ok(Some(Frame::Snapshot { vt: dump })) => {
                    if pending_attach > 1 {
                        pending_attach -= 1; // superseded attach — not our view
                        continue;
                    }
                    pending_attach = 0;
                    if let Some(name) = &attached {
                        let _ = ev_tx.send(Ev::Bytes {
                            name: name.clone(),
                            data: dump,
                            snapshot: true,
                        });
                    }
                }
                Ok(Some(Frame::Output { bytes })) => {
                    if pending_attach > 0 { continue; } // belongs to a session we just left
                    if let Some(name) = &attached {
                        let _ = ev_tx.send(Ev::Bytes {
                            name: name.clone(),
                            data: bytes,
                            snapshot: false,
                        });
                    }
                }
                Ok(Some(Frame::Created { name })) => {
                    let _ = ev_tx.send(Ev::Created(name));
                    let _ = writer.write_frame(&Frame::ListSessions).await;
                }
                Ok(Some(Frame::Error { code, msg })) => {
                    // SESSION_EXITED carries no session name: it can only be
                    // pinned on the current attach when no switch is in
                    // flight. With pending_attach > 0 it belongs to the
                    // session we just left (e.g. it was killed as we switched
                    // away) — taking `attached` then would drop the incoming
                    // Snapshot of the new session.
                    if code == code::SESSION_EXITED {
                        if pending_attach == 0
                            && let Some(name) = attached.take()
                        {
                            let _ = ev_tx.send(Ev::SessionEnded { name, msg });
                        }
                    }
                    // A failed Attach (the session died first) sends this
                    // instead of a Snapshot — drain the count or later
                    // Snapshots would be taken for stale ones.
                    else if code == code::NO_SUCH_SESSION && pending_attach > 0 {
                        pending_attach -= 1;
                    } else {
                        tracing::debug!(code, %msg, "daemon error");
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return Err("connection closed".to_string()),
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::Attach { name, cols, rows }) => {
                    // Switching sessions on one connection means detach first.
                    if attached.is_some() {
                        let _ = writer.write_frame(&Frame::Detach).await;
                    }
                    pending_attach += 1;
                    attached = Some(name.clone());
                    if writer.write_frame(&Frame::Attach { name, cols, rows }).await.is_err() {
                        return Err("attach write failed".to_string());
                    }
                }
                Some(Cmd::Input(bytes)) => {
                    if attached.is_some()
                        && writer.write_frame(&Frame::Input { bytes }).await.is_err()
                    {
                        return Err("input write failed".to_string());
                    }
                }
                Some(Cmd::Resize { cols, rows }) => {
                    if attached.is_some()
                        && writer.write_frame(&Frame::Resize { cols, rows }).await.is_err()
                    {
                        return Err("resize write failed".to_string());
                    }
                }
                Some(Cmd::Create) => {
                    if writer.write_frame(&Frame::Create { name: None, cmd: None }).await.is_err() {
                        return Err("create write failed".to_string());
                    }
                }
                Some(Cmd::Kill { name }) => {
                    if writer.write_frame(&Frame::Kill { name }).await.is_err() {
                        return Err("kill write failed".to_string());
                    }
                    let _ = writer.write_frame(&Frame::ListSessions).await;
                }
                Some(Cmd::Shutdown) | None => {
                    if attached.is_some() {
                        let _ = writer.write_frame(&Frame::Detach).await;
                    }
                    return Ok(());
                }
            },
        }
    }
}
