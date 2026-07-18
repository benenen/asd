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

/// The connection actor's attach bookkeeping, as a small pure state machine so
/// the frame-routing rules — which Snapshot/Output belongs to the current view,
/// and when a switch is still converging — are unit-testable (the
/// "active-session / frame-filter" logic).
///
/// `pending` counts Attach frames whose Snapshot has not arrived yet: while
/// `> 0` a live Output is stale (belongs to a session we just left), and while
/// `> 1` an arriving Snapshot belongs to a superseded attach (a quick switch).
/// `showing` names the session the forwarded frames are tagged with — the
/// current view; the TUI drops frames tagged with anything else.
#[derive(Default, Debug, PartialEq, Eq)]
struct Attach {
    pending: usize,
    showing: Option<String>,
}

impl Attach {
    /// Begin an Attach to `name`. Returns whether the connection was already
    /// attached (so the caller writes a `Detach` first — switching sessions on
    /// one connection is detach-then-attach).
    fn begin(&mut self, name: String) -> bool {
        let was_attached = self.showing.is_some();
        self.pending += 1;
        self.showing = Some(name);
        was_attached
    }

    /// A Snapshot arrived: the session name to tag it with, or `None` when it
    /// belongs to a superseded attach and must be dropped.
    fn on_snapshot(&mut self) -> Option<String> {
        if self.pending > 1 {
            self.pending -= 1; // superseded attach — not our view
            return None;
        }
        self.pending = 0;
        self.showing.clone()
    }

    /// An Output arrived: the session name to tag it with, or `None` while a
    /// switch is still converging (the bytes belong to a session we just left).
    fn on_output(&self) -> Option<String> {
        if self.pending > 0 {
            return None;
        }
        self.showing.clone()
    }

    /// The attached session exited (`SESSION_EXITED` carries no name). Returns
    /// the ended session's name only when it can be pinned on the current view —
    /// with no switch in flight; with one pending, the exit belongs to the
    /// session we just left and taking `showing` would drop the incoming
    /// Snapshot of the new one.
    fn on_session_exited(&mut self) -> Option<String> {
        if self.pending == 0 {
            self.showing.take()
        } else {
            None
        }
    }

    /// A pending Attach failed (`NO_SUCH_SESSION`: the session died before we
    /// attached). Drains one pending count; returns the ended name only if that
    /// was the newest attach, so the client stops holding the pane for a
    /// Snapshot that will never come. Caller guards `pending > 0`.
    fn on_attach_failed(&mut self) -> Option<String> {
        self.pending -= 1;
        if self.pending == 0 {
            self.showing.take()
        } else {
            None
        }
    }
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
                    else if code == code::NO_SUCH_SESSION && at.pending > 0 {
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
                Some(Cmd::Attach { name, cols, rows }) => {
                    // Switching sessions on one connection means detach first.
                    if at.begin(name.clone()) {
                        let _ = writer.write_frame(&Frame::Detach).await;
                    }
                    if writer.write_frame(&Frame::Attach { name, cols, rows }).await.is_err() {
                        return Err("attach write failed".to_string());
                    }
                }
                Some(Cmd::Input(bytes)) => {
                    if at.showing.is_some()
                        && writer.write_frame(&Frame::Input { bytes }).await.is_err()
                    {
                        return Err("input write failed".to_string());
                    }
                }
                Some(Cmd::Resize { cols, rows }) => {
                    if at.showing.is_some()
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
                Some(Cmd::Rename { name, new_name }) => {
                    if writer.write_frame(&Frame::Rename { name, new_name }).await.is_err() {
                        return Err("rename write failed".to_string());
                    }
                    // Refresh the list so the new name shows promptly.
                    let _ = writer.write_frame(&Frame::ListSessions).await;
                }
                Some(Cmd::Shutdown) | None => {
                    if at.showing.is_some() {
                        let _ = writer.write_frame(&Frame::Detach).await;
                    }
                    return Ok(());
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Attach;

    fn s(name: &str) -> Option<String> {
        Some(name.to_string())
    }

    #[test]
    fn first_attach_needs_no_detach_switch_does() {
        let mut at = Attach::default();
        // Nothing attached yet → no Detach precedes the first Attach.
        assert!(!at.begin("a".into()));
        at.on_snapshot(); // converges
        // Now attached → switching sends a Detach first.
        assert!(at.begin("b".into()));
    }

    #[test]
    fn snapshot_then_output_tag_the_current_view() {
        let mut at = Attach::default();
        at.begin("a".into());
        assert_eq!(at.on_snapshot(), s("a")); // reveal a
        assert_eq!(at.on_output(), s("a")); // live output belongs to a
    }

    #[test]
    fn output_is_dropped_until_the_snapshot_converges() {
        let mut at = Attach::default();
        at.begin("a".into());
        // A switch is in flight: Output before the Snapshot is stale, dropped.
        assert_eq!(at.on_output(), None);
        assert_eq!(at.on_snapshot(), s("a"));
        assert_eq!(at.on_output(), s("a"));
    }

    #[test]
    fn quick_switch_drops_the_superseded_snapshot() {
        let mut at = Attach::default();
        at.begin("a".into());
        at.begin("b".into()); // switched before a's snapshot arrived
        // a's snapshot arrives first — superseded, dropped (not shown as b).
        assert_eq!(at.on_snapshot(), None);
        // b's snapshot arrives — this is the view.
        assert_eq!(at.on_snapshot(), s("b"));
        assert_eq!(at.on_output(), s("b"));
    }

    #[test]
    fn session_exit_pins_the_name_only_when_settled() {
        // No switch in flight: the exit is the current view's.
        let mut at = Attach::default();
        at.begin("a".into());
        at.on_snapshot();
        assert_eq!(at.on_session_exited(), s("a"));
        // After the exit nothing is shown.
        assert_eq!(at.on_output(), None);

        // Switch in flight: a stray exit belongs to the session we left, so it
        // must NOT take the pending view (that would drop the new Snapshot).
        let mut at = Attach::default();
        at.begin("a".into());
        at.on_snapshot();
        at.begin("b".into()); // switching to b, snapshot pending
        assert_eq!(at.on_session_exited(), None);
        assert_eq!(at.on_snapshot(), s("b")); // b still reveals
    }

    #[test]
    fn failed_attach_drains_and_reports_only_the_newest() {
        // Single failed attach → report so the pane stops holding.
        let mut at = Attach::default();
        at.begin("gone".into());
        assert_eq!(at.on_attach_failed(), s("gone"));

        // Two in flight, the older attach fails: drain the count but keep the
        // newer view; its Snapshot still reveals.
        let mut at = Attach::default();
        at.begin("a".into());
        at.begin("b".into());
        assert_eq!(at.on_attach_failed(), None); // a failed, b still pending
        assert_eq!(at.on_snapshot(), s("b"));
    }

    /// Bug regression (client side): after the attached session is killed and
    /// exits, attaching a brand-new session routes that session's Snapshot to
    /// the pane — the frame is NOT filtered out. (The daemon-side fix is what
    /// makes the Snapshot actually arrive; this pins the client's routing.)
    #[test]
    fn reattach_after_kill_routes_the_new_sessions_snapshot() {
        let mut at = Attach::default();
        at.begin("a".into());
        assert_eq!(at.on_snapshot(), s("a"));
        // a is killed and exits under us.
        assert_eq!(at.on_session_exited(), s("a"));
        assert_eq!(at.showing, None);
        // Create + attach a new session b.
        assert!(!at.begin("b".into())); // not attached (a exited) → no Detach
        // b's Snapshot must reveal as b — not dropped, not tagged a.
        assert_eq!(at.on_snapshot(), s("b"));
        assert_eq!(at.on_output(), s("b"));
    }
}
