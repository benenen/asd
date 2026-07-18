//! Per-host connection actors. ghostty-web consumes the raw PTY bytes (no
//! local VT), so each host actor is a plain tokio task that speaks the framed
//! protocol and forwards bytes.
//!
//! Each host — the local daemon or an SSH remote — gets one actor that:
//!   * handshakes, then polls `ListSessions` on an interval → the sidebar;
//!   * while attached, forwards Snapshot/Output bytes tagged with the session
//!     they belong to;
//!   * obeys [`HostCmd`]s (attach/detach/input/resize/create/kill).
//!
//! The transport is boxed so one `drive` loop serves both a local `UnixStream`
//! and a remote russh `ChannelStream` (see [`crate::ssh`]).

use std::time::Duration;

use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, PROTO_VERSION, code};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::model::{HostId, HostKind, HostState};

/// How often each host re-polls its session list.
const LIST_INTERVAL: Duration = Duration::from_millis(1500);

/// A boxed transport half, so local and SSH connections share one code path.
pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// Commands the supervisor sends to a single host actor.
#[derive(Debug, Clone)]
pub enum HostCmd {
    /// Attach to (or switch to) `name`, sizing the pty to `cols`×`rows`.
    Attach {
        name: String,
        cols: u16,
        rows: u16,
    },
    /// Stop viewing the current session (stay connected for the list).
    Detach,
    /// Raw input bytes for the attached session (ghostty-web already encoded
    /// keys/mouse — no client-side key encoding needed).
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
    /// Rename session `name` to `new_name` (daemon validates + acks).
    Rename {
        name: String,
        new_name: String,
    },
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
    /// PTY bytes for the session named `name`. The app drops bytes whose
    /// session is no longer the active one (stale frames in flight across a
    /// switch). `snapshot` marks the full attach dump: the app resets the
    /// terminal before writing it.
    Bytes {
        host: HostId,
        name: String,
        data: Vec<u8>,
        snapshot: bool,
    },
    SessionEnded {
        host: HostId,
        name: String,
        msg: String,
    },
}

/// Task entry point for one host: establish the transport and drive the
/// connection to completion. A failure is reported as a `Down` state.
pub async fn run_host(
    id: HostId,
    kind: HostKind,
    cmd_rx: UnboundedReceiver<HostCmd>,
    ev_tx: UnboundedSender<UiEvent>,
) {
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
}

/// Open the local daemon socket and box the halves. Unix only: the local
/// daemon speaks over a Unix socket (tokio has no `UnixStream` on Windows),
/// and the Windows client is GUI-only with no bundled daemon — it reaches
/// sessions through SSH remotes instead.
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

    // Attach frames sent whose Snapshot has not arrived yet. While > 0, Output
    // belongs to a session we already left and is dropped. While > 1, arriving
    // Snapshots belong to superseded attaches (the user switched again before
    // the reply landed) and are dropped too — feeding them would paint the old
    // session over the new one (the A→B→A switch scramble; all asd clients
    // guard it the same way).
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
                    let _ = ev_tx.send(UiEvent::Sessions { host: id, sessions });
                }
                Ok(Some(Frame::Snapshot { vt: dump })) => {
                    if pending_attach > 1 {
                        pending_attach -= 1; // superseded attach — not our view
                        continue;
                    }
                    pending_attach = 0;
                    if let Some(name) = &attached {
                        let _ = ev_tx.send(UiEvent::Bytes {
                            host: id,
                            name: name.clone(),
                            data: dump,
                            snapshot: true,
                        });
                    }
                }
                Ok(Some(Frame::Output { bytes })) => {
                    if pending_attach > 0 { continue; } // belongs to a session we just left
                    if let Some(name) = &attached {
                        let _ = ev_tx.send(UiEvent::Bytes {
                            host: id,
                            name: name.clone(),
                            data: bytes,
                            snapshot: false,
                        });
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
                    // pending_attach stays: any Snapshot still in flight must
                    // drain through the Snapshot branch (attached is None,
                    // nothing is forwarded) so the count stays aligned.
                }
                Some(HostCmd::Input(bytes)) => {
                    if attached.is_some()
                        && writer.write_frame(&Frame::Input { bytes }).await.is_err()
                    {
                        return Err("input write failed".to_string());
                    }
                }
                Some(HostCmd::Resize { cols, rows }) => {
                    if attached.is_some()
                        && writer.write_frame(&Frame::Resize { cols, rows }).await.is_err()
                    {
                        return Err("resize write failed".to_string());
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
                Some(HostCmd::Rename { name, new_name }) => {
                    if writer.write_frame(&Frame::Rename { name, new_name }).await.is_err() {
                        return Err("rename write failed".to_string());
                    }
                    // Refresh promptly so the new name shows even if the
                    // optimistic local update was reverted.
                    let _ = writer.write_frame(&Frame::ListSessions).await;
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
