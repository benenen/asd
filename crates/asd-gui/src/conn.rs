//! Per-host connection actors (spec §7, extended for M2 multi-host).
//!
//! Each host — the local daemon or an SSH remote — gets one std thread with its
//! own current-thread tokio runtime and a `!Send` [`GhosttyVt`] (kept off iced's
//! multi-threaded runtime). The actor:
//!   * handshakes, then polls `ListSessions` on an interval → the sidebar;
//!   * while attached, feeds Snapshot/Output into the terminal → [`RenderSnapshot`]s;
//!   * obeys [`HostCmd`]s (attach/detach/key/resize/create/kill).
//!
//! The transport is boxed so one `drive` loop serves both a local `UnixStream`
//! and a remote russh `ChannelStream` (see [`crate::ssh`]). Only plain `Send`
//! data ([`UiEvent`]) leaves the thread.

use std::time::Duration;

use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, PROTO_VERSION, code};
use asd_vt::{GhosttyVt, KeyEvent, RenderSnapshot, VtBackend};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::model::{HostId, HostKind, HostState};

/// Scrollback kept by each attached terminal.
const SCROLLBACK: usize = 10_000;
/// How often each host re-polls its session list.
const LIST_INTERVAL: Duration = Duration::from_millis(1500);

/// A boxed transport half, so local and SSH connections share one code path.
pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// Commands the supervisor sends to a single host actor.
#[derive(Debug, Clone)]
pub enum HostCmd {
    /// Attach to (or switch to) `name`, sizing the view to `cols`×`rows`.
    Attach {
        name: String,
        cols: u16,
        rows: u16,
    },
    /// Stop viewing the current session (stay connected for the list).
    Detach,
    Key(KeyEvent),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Create a new session (daemon auto-names it).
    Create,
    Kill {
        name: String,
    },
    /// Set the scrollback viewport offset (0 = follow live output).
    Scroll(usize),
    /// Disconnect and end the actor.
    Shutdown,
}

/// Events a host actor sends toward the app, tagged with its host id.
#[derive(Debug, Clone)]
pub enum UiEvent {
    State {
        host: HostId,
        state: HostState,
    },
    Sessions {
        host: HostId,
        sessions: Vec<asd_proto::SessionInfo>,
    },
    /// A `Create` completed; the app may auto-select `name`.
    Created {
        host: HostId,
        name: String,
    },
    Frame {
        host: HostId,
        /// Session this frame renders; the app drops frames whose session is
        /// no longer the active one (stale frames in flight across a switch).
        name: String,
        snap: Box<RenderSnapshot>,
        /// Whether the session program is currently tracking the mouse
        /// (vim/htop): the GUI forwards mouse events instead of scrolling
        /// or selecting locally.
        session_wants_mouse: bool,
        /// Absolute row of the viewport's top line for this frame:
        /// `scrollback_rows − scroll` (0 = oldest scrollback line). The app
        /// anchors text selections in this absolute space so the highlight
        /// tracks the text — not a screen position — while scrolling.
        base: usize,
    },
    SessionEnded {
        host: HostId,
        name: String,
        msg: String,
    },
}

/// Thread entry point for one host. Builds a current-thread runtime and drives
/// the connection to completion.
pub fn run_host(
    id: HostId,
    kind: HostKind,
    cmd_rx: UnboundedReceiver<HostCmd>,
    ev_tx: UnboundedSender<UiEvent>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ev_tx.send(UiEvent::State {
                host: id,
                state: HostState::Down(format!("runtime: {e}")),
            });
            return;
        }
    };
    runtime.block_on(async move {
        // Establish the transport; a failure is a down state, not a panic.
        let opened = match &kind {
            HostKind::Local => connect_local().await,
            HostKind::Ssh(spec) => crate::ssh::open(spec).await,
        };
        let (reader, writer) = match opened {
            Ok(rw) => rw,
            Err(e) => {
                let _ = ev_tx.send(UiEvent::State {
                    host: id,
                    state: HostState::Down(e.to_string()),
                });
                return;
            }
        };
        if let Err(reason) = drive(id, reader, writer, cmd_rx, &ev_tx).await {
            let _ = ev_tx.send(UiEvent::State {
                host: id,
                state: HostState::Down(reason),
            });
        }
    });
}

/// Open the local daemon socket and box the halves. Unix only: the local
/// daemon speaks over a Unix socket (tokio has no `UnixStream` on Windows), and
/// the Windows client is GUI-only with no bundled daemon — it reaches sessions
/// through SSH remotes instead.
#[cfg(unix)]
async fn connect_local() -> anyhow::Result<(BoxRead, BoxWrite)> {
    let socket = asd_proto::paths::socket_path();
    let stream = tokio::net::UnixStream::connect(&socket)
        .await
        .map_err(|e| anyhow::anyhow!("connect {}: {e}", socket.display()))?;
    let (r, w) = tokio::io::split(stream);
    Ok((Box::new(r), Box::new(w)))
}

#[cfg(not(unix))]
async fn connect_local() -> anyhow::Result<(BoxRead, BoxWrite)> {
    anyhow::bail!("no local daemon on this platform — connect an SSH remote instead")
}

/// The per-host event loop. Returns `Err(reason)` if the connection ends
/// abnormally; a clean `Shutdown` returns `Ok(())`.
async fn drive(
    id: HostId,
    reader: BoxRead,
    writer: BoxWrite,
    mut cmd_rx: UnboundedReceiver<HostCmd>,
    ev_tx: &UnboundedSender<UiEvent>,
) -> Result<(), String> {
    let mut reader = FrameReader::new(reader);
    let mut writer = FrameWriter::new(writer);

    // Handshake.
    writer
        .write_frame(&Frame::Hello {
            proto_version: PROTO_VERSION,
            kind: ClientKind::Gui,
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
    let _ = ev_tx.send(UiEvent::State {
        host: id,
        state: HostState::Up,
    });

    // Terminal for the active attach (None while not viewing this host).
    let mut vt: Option<GhosttyVt> = None;
    // Attach frames sent whose Snapshot has not arrived yet. While > 0, Output
    // belongs to a session we already left and is dropped. While > 1, arriving
    // Snapshots belong to superseded attaches (the user switched again before
    // the reply landed) and are dropped too — feeding them would paint the old
    // session over the new one and stack both scrollbacks (a plain bool here
    // let a quick A→B→A switch scramble text and selection coordinates).
    let mut pending_attach: usize = 0;
    // Scrollback offset: 0 = follow live output, >0 = lines scrolled up.
    let mut scroll: usize = 0;
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
                    let _ = ev_tx.send(UiEvent::Sessions { host: id, sessions });
                }
                Ok(Some(Frame::Snapshot { vt: dump })) => {
                    if pending_attach > 1 {
                        pending_attach -= 1; // superseded attach — not our view
                        continue;
                    }
                    pending_attach = 0;
                    if let Some(term) = &mut vt {
                        term.feed(&dump);
                        let _ = term.take_pty_responses();
                        let wants_mouse = term.is_mouse_tracking();
                        term.set_scroll(scroll);
                        let base = term.scrollback_rows().saturating_sub(scroll);
                        let _ = ev_tx.send(UiEvent::Frame { host: id, name: attached.clone().unwrap_or_default(), snap: Box::new(term.render_snapshot()), session_wants_mouse: wants_mouse, base });
                    }
                }
                Ok(Some(Frame::Output { bytes })) => {
                    if pending_attach > 0 { continue; } // belongs to a session we just left
                    if let Some(term) = &mut vt {
                        term.feed(&bytes);
                        let _ = term.take_pty_responses();
                        let wants_mouse = term.is_mouse_tracking();
                        term.set_scroll(scroll);
                        let base = term.scrollback_rows().saturating_sub(scroll);
                        let _ = ev_tx.send(UiEvent::Frame { host: id, name: attached.clone().unwrap_or_default(), snap: Box::new(term.render_snapshot()), session_wants_mouse: wants_mouse, base });
                    }
                }
                Ok(Some(Frame::Created { name })) => {
                    let _ = ev_tx.send(UiEvent::Created { host: id, name });
                    // Refresh the list promptly so the new session shows up.
                    let _ = writer.write_frame(&Frame::ListSessions).await;
                }
                Ok(Some(Frame::Error { code, msg })) => {
                    // SESSION_EXITED carries no session name: only pin it on
                    // the current attach when no switch is in flight. With
                    // pending_attach > 0 it belongs to the session we just
                    // left — taking `attached` then would drop the incoming
                    // Snapshot of the new session.
                    if code == code::SESSION_EXITED {
                        if pending_attach == 0
                            && let Some(name) = attached.take()
                        {
                            vt = None;
                            let _ = ev_tx.send(UiEvent::SessionEnded { host: id, name, msg });
                        }
                    }
                    // A failed Attach (the session died before the daemon saw
                    // it) sends this instead of a Snapshot — drain the count
                    // or every later Snapshot would be taken for a stale one.
                    else if code == code::NO_SUCH_SESSION && pending_attach > 0 {
                        pending_attach -= 1;
                    }
                    // Other errors are logged and ignored; the next list poll
                    // reconciles.
                    else {
                        tracing::debug!(host = id, code, %msg, "daemon error");
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return Err("connection closed".to_string()),
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(HostCmd::Attach { name, cols, rows }) => {
                    // Switching sessions on one connection means detach first.
                    if attached.is_some() {
                        let _ = writer.write_frame(&Frame::Detach).await;
                    }
                    vt = Some(GhosttyVt::new(cols.max(1), rows.max(1), SCROLLBACK));
                    scroll = 0;
                    pending_attach += 1;
                    attached = Some(name.clone());
                    if writer.write_frame(&Frame::Attach { name, cols, rows }).await.is_err() {
                        return Err("attach write failed".to_string());
                    }
                }
                Some(HostCmd::Detach) => {
                    if attached.take().is_some() {
                        let _ = writer.write_frame(&Frame::Detach).await;
                    }
                    vt = None;
                    // pending_attach stays: any Snapshot still in flight must
                    // drain through the Snapshot branch (vt is None, nothing
                    // renders) so the count stays aligned with the stream.
                }
                Some(HostCmd::Key(ev)) => {
                    if let Some(term) = &mut vt {
                        let bytes = term.encode_key(ev);
                        if !bytes.is_empty()
                            && writer.write_frame(&Frame::Input { bytes }).await.is_err()
                        {
                            return Err("input write failed".to_string());
                        }
                    }
                }
                Some(HostCmd::Resize { cols, rows }) => {
                    if let Some(term) = &mut vt {
                        term.resize(cols.max(1), rows.max(1));
                        if writer.write_frame(&Frame::Resize { cols, rows }).await.is_err() {
                            return Err("resize write failed".to_string());
                        }
                        let wants_mouse = term.is_mouse_tracking();
                        term.set_scroll(scroll);
                        let base = term.scrollback_rows().saturating_sub(scroll);
                        let _ = ev_tx.send(UiEvent::Frame { host: id, name: attached.clone().unwrap_or_default(), snap: Box::new(term.render_snapshot()), session_wants_mouse: wants_mouse, base });
                    }
                }
                Some(HostCmd::Create) => {
                    if writer.write_frame(&Frame::Create { name: None, cmd: None }).await.is_err() {
                        return Err("create write failed".to_string());
                    }
                }
                Some(HostCmd::Kill { name }) => {
                    if writer.write_frame(&Frame::Kill { name }).await.is_err() {
                        return Err("kill write failed".to_string());
                    }
                    let _ = writer.write_frame(&Frame::ListSessions).await;
                }
                Some(HostCmd::Scroll(lines)) => {
                    scroll = lines;
                    if let Some(term) = &mut vt {
                        let wants_mouse = term.is_mouse_tracking();
                        term.set_scroll(scroll);
                        let base = term.scrollback_rows().saturating_sub(scroll);
                        let _ = ev_tx.send(UiEvent::Frame { host: id, name: attached.clone().unwrap_or_default(), snap: Box::new(term.render_snapshot()), session_wants_mouse: wants_mouse, base });
                    }
                }
                Some(HostCmd::Shutdown) | None => {
                    if attached.is_some() {
                        let _ = writer.write_frame(&Frame::Detach).await;
                    }
                    return Ok(());
                }
            },
        }
    }
}

/// A supervisor-side handle to one running host actor.
pub struct HostHandle {
    pub cmd_tx: UnboundedSender<HostCmd>,
}
