//! Daemon connection on a background thread (mirrors asd-dioxus's host actor,
//! local-only): the TUI thread owns the `!Send` terminal, so only plain data
//! crosses the two std channels here.
//!
//! The actor handshakes, polls `ListSessions` for the sidebar, and while
//! attached forwards raw Snapshot/Output bytes tagged with the session they
//! belong to. The `pending_attach` counter drops frames of superseded attaches
//! so a quick session switch can't paint stale content (same race as the GUI
//! clients).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{SendError, Sender};
use std::time::Duration;

use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, TerminalAppearance, code};
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
        appearance: TerminalAppearance,
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
    /// Rename session `name` to `new_name`.
    Rename {
        name: String,
        new_name: String,
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
    /// A `Rename` completed (`Ok`) or was rejected by the daemon (`Err(msg)`).
    Renamed(Result<(), String>),
}

/// An actor event tagged with the connection generation that produced it.
/// Reconnect starts a new generation, so buffered events from the superseded
/// actor cannot overwrite the new connection's state.
#[derive(Debug)]
pub struct ConnectionEvent {
    pub generation: u64,
    pub event: Ev,
}

#[derive(Clone)]
struct EventSink {
    generation: u64,
    tx: Sender<ConnectionEvent>,
}

impl EventSink {
    fn send(&self, event: Ev) -> Result<(), SendError<ConnectionEvent>> {
        self.tx.send(ConnectionEvent {
            generation: self.generation,
            event,
        })
    }
}

use asd_client::attach::Attach;

/// Handle to the running actor thread.
pub struct Conn {
    pub cmd_tx: UnboundedSender<Cmd>,
}

impl Conn {
    /// Spawn the actor thread with its own current-thread runtime. Events —
    /// including connect/handshake failures — arrive on `ev_tx`.
    pub fn spawn(socket: PathBuf, generation: u64, ev_tx: Sender<ConnectionEvent>) -> Self {
        let (cmd_tx, cmd_rx) = unbounded_channel::<Cmd>();
        let events = EventSink {
            generation,
            tx: ev_tx,
        };
        std::thread::Builder::new()
            .name("asd-tui-conn".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = events.send(Ev::Down(format!("runtime: {e}")));
                        return;
                    }
                };
                rt.block_on(async move {
                    if let Err(reason) = drive(&socket, cmd_rx, &events).await {
                        let _ = events.send(Ev::Down(reason));
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
    socket: &Path,
    mut cmd_rx: UnboundedReceiver<Cmd>,
    ev_tx: &EventSink,
) -> Result<(), String> {
    let (r, w) = crate::platform::connect_stream(socket).await?;
    let mut reader = FrameReader::new(r);
    let mut writer = FrameWriter::new(w);

    asd_client::handshake(&mut writer, &mut reader, ClientKind::Cli)
        .await
        .map_err(|msg| format!("handshake: {msg}"))?;
    let _ = ev_tx.send(Ev::Up);

    // Attach bookkeeping (see `Attach`): which session's frames to forward and
    // whether a switch is still converging.
    let mut at = Attach::default();

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
                    if let Some(name) = at.on_snapshot() {
                        let _ = ev_tx.send(Ev::Bytes {
                            name,
                            data: dump,
                            snapshot: true,
                        });
                    }
                }
                Ok(Some(Frame::Output { bytes })) => {
                    if let Some(name) = at.on_output() {
                        let _ = ev_tx.send(Ev::Bytes {
                            name,
                            data: bytes,
                            snapshot: false,
                        });
                    }
                }
                Ok(Some(Frame::Created { name })) => {
                    let _ = ev_tx.send(Ev::Created(name));
                    let _ = writer.write_frame(&Frame::ListSessions).await;
                }
                // The only `Ack` this client can receive is a Rename success.
                Ok(Some(Frame::Ack)) => {
                    let _ = ev_tx.send(Ev::Renamed(Ok(())));
                }
                Ok(Some(Frame::Error { code, msg })) => {
                    // SESSION_EXITED carries no session name: it can only be
                    // pinned on the current attach when no switch is in
                    // flight. With pending_attach > 0 it belongs to the
                    // session we just left (e.g. it was killed as we switched
                    // away) — taking `attached` then would drop the incoming
                    // Snapshot of the new session.
                    if code == code::SESSION_EXITED {
                        if let Some(name) = at.on_session_exited() {
                            let _ = ev_tx.send(Ev::SessionEnded { name, msg });
                        }
                    }
                    // A failed Attach (the session died first) sends this
                    // instead of a Snapshot — drain the count or later
                    // Snapshots would be taken for stale ones. When it was
                    // the newest attach that failed, tell the TUI so it
                    // stops holding the pane for a Snapshot that will never
                    // come.
                    else if code == code::NO_SUCH_SESSION && at.pending() > 0 {
                        if let Some(name) = at.on_attach_failed() {
                            let _ = ev_tx.send(Ev::SessionEnded { name, msg });
                        }
                    }
                    // Rename rejections (only this client's Rename produces
                    // these codes: bad name, or the target name already taken).
                    else if code == code::INVALID_NAME || code == code::SESSION_EXISTS {
                        let _ = ev_tx.send(Ev::Renamed(Err(msg)));
                    } else {
                        tracing::debug!(code, %msg, "daemon error");
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return Err("connection closed".to_string()),
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::Attach {
                    name,
                    cols,
                    rows,
                    appearance,
                }) => {
                    // Switching sessions on one connection means detach first.
                    if at.begin(name.clone()) {
                        let _ = writer.write_frame(&Frame::Detach).await;
                    }
                    if writer.write_frame(&Frame::Attach { name, cols, rows, appearance }).await.is_err() {
                        return Err("attach write failed".to_string());
                    }
                }
                Some(Cmd::Input(bytes)) => {
                    if at.is_attached()
                        && writer.write_frame(&Frame::Input { bytes }).await.is_err()
                    {
                        return Err("input write failed".to_string());
                    }
                }
                Some(Cmd::Resize { cols, rows }) => {
                    if at.is_attached()
                        && writer.write_frame(&Frame::Resize { cols, rows }).await.is_err()
                    {
                        return Err("resize write failed".to_string());
                    }
                }
                Some(Cmd::Create) => {
                    if writer.write_frame(&Frame::Create { name: None, cmd: None, cwd: None }).await.is_err() {
                        return Err("create write failed".to_string());
                    }
                }
                Some(Cmd::Kill { name }) => {
                    if writer.write_frame(&Frame::Kill { name }).await.is_err() {
                        return Err("kill write failed".to_string());
                    }
                    let _ = writer.write_frame(&Frame::ListSessions).await;
                }
                Some(Cmd::Rename { name, new_name }) => {
                    // Keep the frame tag in step with the TUI's optimistic
                    // rename of its active session (see `Attach::on_rename`).
                    at.on_rename(&name, &new_name);
                    if writer.write_frame(&Frame::Rename { name, new_name }).await.is_err() {
                        return Err("rename write failed".to_string());
                    }
                    // Refresh the list so the new name shows promptly.
                    let _ = writer.write_frame(&Frame::ListSessions).await;
                }
                Some(Cmd::Shutdown) | None => {
                    if at.is_attached() {
                        let _ = writer.write_frame(&Frame::Detach).await;
                    }
                    return Ok(());
                }
            },
        }
    }
}
