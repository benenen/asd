//! Per-host connection actors. ghostty-web consumes the raw PTY bytes (no
//! local VT), so each host actor is a plain tokio task that speaks the framed
//! protocol and forwards bytes.
//!
//! Each host — the local daemon or an SSH remote — gets one actor that:
//!   * handshakes, then subscribes on a dedicated event transport → the sidebar;
//!   * while attached, forwards Snapshot/Output bytes tagged with the session
//!     they belong to;
//!   * obeys [`HostCmd`]s (attach/detach/input/resize/create/kill).
//!
//! The transport is boxed so one `drive` loop serves both the local platform
//! stream and a remote russh `ChannelStream` (see [`crate::ssh`]).

use asd_client::attach::Attach;
use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, code};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::model::{HostId, HostKind, HostState};

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
        identity: asd_proto::SessionIdentity,
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
    Events {
        host: HostId,
        cursor: asd_proto::EventCursor,
        change: asd_client::events::EventFeedChange,
    },
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
        identity: asd_proto::SessionIdentity,
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
        HostKind::Local => crate::platform::connect_local().await,
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
    let (feed_tx, feed_rx) = tokio::sync::mpsc::unbounded_channel();
    let events = asd_client::event_transport::watch(
        || async {
            match &kind {
                HostKind::Local => crate::platform::connect_local().await,
                HostKind::Ssh(spec) => crate::ssh::open(spec).await,
            }
            .map_err(|e| e.to_string())
        },
        ClientKind::Gui,
        |cursor, change| {
            let _ = feed_tx.send((cursor, change));
        },
    );
    let result = tokio::select! {
        result = drive(id, reader, writer, cmd_rx, &ev_tx, feed_rx) => result,
        result = events => result,
    };
    if let Err(reason) = result {
        let _ = ev_tx.send(UiEvent::State {
            host: id,
            state: HostState::Down(reason),
        });
    }
}

/// The per-host event loop. Returns `Err(reason)` if the connection ends
/// abnormally; a clean `Shutdown` returns `Ok(())`.
async fn drive(
    id: HostId,
    reader: BoxRead,
    writer: BoxWrite,
    mut cmd_rx: UnboundedReceiver<HostCmd>,
    ev_tx: &UnboundedSender<UiEvent>,
    mut feed_rx: UnboundedReceiver<(asd_proto::EventCursor, asd_client::events::EventFeedChange)>,
) -> Result<(), String> {
    let mut reader = FrameReader::new(reader);
    let mut writer = FrameWriter::new(writer);

    // Handshake.
    asd_client::handshake(&mut writer, &mut reader, ClientKind::Gui)
        .await
        .map_err(|msg| format!("handshake: {msg}"))?;
    let _ = ev_tx.send(UiEvent::State {
        host: id,
        state: HostState::Up,
    });

    // Attach state machine (shared with asd-tui; see asd_client::attach::Attach).
    let mut at = Attach::default();

    loop {
        tokio::select! {
            Some((cursor, change)) = feed_rx.recv() => {
                if let asd_client::events::EventFeedChange::Reset { sessions, .. }
                    | asd_client::events::EventFeedChange::Changed { sessions, .. } = &change
                    && let Some(attached) = at.on_output()
                    && let Some(info) = sessions.iter().find(|s| s.identity() == attached.identity) {
                    at.on_rename(&attached.name, &info.name);
                }
                let _ = ev_tx.send(UiEvent::Events { host: id, cursor, change });
            }
            frame = reader.read_frame() => match frame {
                Ok(Some(Frame::Snapshot { identity, vt: dump })) => {
                    if let Some(attached) = at.on_snapshot(identity) {
                        let _ = ev_tx.send(UiEvent::Bytes {
                            host: id,
                            name: attached.name,
                            identity: attached.identity,
                            data: dump,
                            snapshot: true,
                        });
                    }
                }
                Ok(Some(Frame::Output { bytes })) => {
                    if let Some(attached) = at.on_output() {
                        let _ = ev_tx.send(UiEvent::Bytes {
                            host: id,
                            name: attached.name,
                            identity: attached.identity,
                            data: bytes,
                            snapshot: false,
                        });
                    }
                }
                Ok(Some(Frame::Created { name })) => {
                    let _ = ev_tx.send(UiEvent::Created { host: id, name });
                }
                Ok(Some(Frame::Error { code, msg })) => {
                    // SESSION_EXITED carries no session name: only pin it on
                    // the current attach when no switch is in flight. With a
                    // pending attach it belongs to the session we just left —
                    // taking the shown name then would drop the incoming
                    // Snapshot of the new session.
                    if code == code::SESSION_EXITED {
                        if let Some(name) = at.on_session_exited() {
                            let _ = ev_tx.send(UiEvent::SessionEnded { host: id, name, msg });
                        }
                    }
                    // A failed Attach (the session died before the daemon saw
                    // it) sends this instead of a Snapshot — drain the count or
                    // every later Snapshot would be taken for a stale one. When
                    // that was the newest attach the view is now showing
                    // nothing, so tell the UI rather than leaving the pane
                    // waiting on a Snapshot that will never arrive.
                    else if code == code::NO_SUCH_SESSION && at.pending() > 0 {
                        if let Some(name) = at.on_attach_failed() {
                            let _ = ev_tx.send(UiEvent::SessionEnded { host: id, name, msg });
                        }
                    }
                    // Other errors are logged; accepted events reconcile facts.
                    else {
                        tracing::debug!(host = id, code, %msg, "daemon error");
                    }
                }
                Ok(Some(_)) => {}
                // Same split as the TUI: a hangup and a broken stream are
                // different faults and the host row shows whichever it was.
                Ok(None) => return Err("daemon closed the connection".to_string()),
                Err(e) => return Err(format!("connection error: {e}")),
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(HostCmd::Attach { name, cols, rows }) => {
                    if at.begin(name.clone()) {
                        let _ = writer.write_frame(&Frame::Detach).await;
                    }
                    if writer.write_frame(&Frame::Attach {
                        name,
                        cols,
                        rows,
                        view_id: 0,
                        appearance: crate::theme::TERMINAL_APPEARANCE,
                        read_only: false,
                    }).await.is_err() {
                        return Err("attach write failed".to_string());
                    }
                }
                Some(HostCmd::Detach) => {
                    if at.detach().is_some() {
                        let _ = writer.write_frame(&Frame::Detach).await;
                    }
                }
                Some(HostCmd::Input(bytes)) => {
                    if at.is_attached()
                        && writer.write_frame(&Frame::Input { bytes }).await.is_err()
                    {
                        return Err("input write failed".to_string());
                    }
                }
                Some(HostCmd::Resize { cols, rows }) => {
                    if at.is_attached()
                        && writer.write_frame(&Frame::Resize { cols, rows }).await.is_err()
                    {
                        return Err("resize write failed".to_string());
                    }
                }
                Some(HostCmd::Create) => {
                    if writer.write_frame(&Frame::Create { name: None, cmd: None, cwd: None }).await.is_err() {
                        return Err("create write failed".to_string());
                    }
                }
                Some(HostCmd::Kill { name, identity }) => {
                    if writer.write_frame(&Frame::Kill { name, identity }).await.is_err() {
                        return Err("kill write failed".to_string());
                    }
                }
                Some(HostCmd::Rename { name, new_name }) => {
                    if writer.write_frame(&Frame::Rename { name, new_name }).await.is_err() {
                        return Err("rename write failed".to_string());
                    }
                }
                Some(HostCmd::Shutdown) | None => {
                    if at.is_attached() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn control_ignores_stale_lists_and_event_rename_retags_output() {
        use asd_client::events::EventFeedChange;
        use std::time::Duration;
        let (client, server) = tokio::io::duplex(8192);
        let (cr, cw) = tokio::io::split(client);
        let (sr, sw) = tokio::io::split(server);
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let (feed_tx, feed_rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = tokio::spawn(async move {
            drive(0, Box::new(cr), Box::new(cw), cmd_rx, &ev_tx, feed_rx).await
        });
        let mut reader = FrameReader::new(sr);
        let mut writer = FrameWriter::new(sw);
        assert!(matches!(
            reader.read_frame().await.unwrap(),
            Some(Frame::Hello {
                kind: ClientKind::Gui,
                ..
            })
        ));
        writer
            .write_frame(&Frame::HelloAck {
                proto_version: asd_proto::PROTO_VERSION,
                daemon_version: "test".into(),
            })
            .await
            .unwrap();
        assert!(matches!(
            ev_rx.recv().await,
            Some(UiEvent::State {
                state: HostState::Up,
                ..
            })
        ));
        cmd_tx
            .send(HostCmd::Attach {
                name: "old".into(),
                cols: 80,
                rows: 24,
            })
            .unwrap();
        assert!(matches!(
            reader.read_frame().await.unwrap(),
            Some(Frame::Attach { .. })
        ));
        let identity = asd_proto::SessionIdentity { instance_id: 1 };
        writer
            .write_frame(&Frame::Snapshot {
                identity,
                vt: b"snapshot".to_vec(),
            })
            .await
            .unwrap();
        assert!(matches!(
            ev_rx.recv().await,
            Some(UiEvent::Bytes { snapshot: true, .. })
        ));
        let info = asd_proto::SessionInfo {
            name: "renamed".into(),
            instance_id: 1,
            command: "sh".into(),
            title: String::new(),
            status_line: String::new(),
            created_ms: 0,
            idle_ms: 0,
            running: false,
            state: asd_proto::AgentState::Unknown,
            attached_clients: 1,
            pid: 1,
            cols: 80,
            rows: 24,
        };
        feed_tx
            .send((
                asd_proto::EventCursor {
                    daemon_epoch: [1; 16],
                    sequence: 1,
                },
                EventFeedChange::Reset {
                    sessions: vec![info],
                    notification_lease: true,
                },
            ))
            .unwrap();
        assert!(matches!(ev_rx.recv().await, Some(UiEvent::Events { .. })));
        writer
            .write_frame(&Frame::SessionList { sessions: vec![] })
            .await
            .unwrap();
        writer
            .write_frame(&Frame::Output {
                bytes: b"current".to_vec(),
            })
            .await
            .unwrap();
        assert!(
            matches!(ev_rx.recv().await, Some(UiEvent::Bytes { name, identity: id, snapshot: false, .. }) if name == "renamed" && id == identity)
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(40), ev_rx.recv())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(40), reader.read_frame())
                .await
                .is_err()
        );
        cmd_tx.send(HostCmd::Shutdown).unwrap();
        assert!(matches!(
            reader.read_frame().await.unwrap(),
            Some(Frame::Detach)
        ));
        assert_eq!(actor.await.unwrap(), Ok(()));
    }

    #[test]
    fn embedded_web_theme_reads_the_shared_css_properties() {
        let bridge = include_str!("../assets/bridge.js");
        let css = include_str!("../assets/app.css");

        assert!(bridge.contains("getPropertyValue('--asd-terminal-background')"));
        assert!(bridge.contains("getPropertyValue('--asd-terminal-foreground')"));
        assert!(css.contains("var(--asd-terminal-background)"));
        assert!(css.contains("var(--asd-terminal-foreground)"));
        assert!(!bridge.contains("#0B0D11"));
        assert!(!bridge.contains("#E7E2D6"));
        assert!(!css.contains("#0B0D11"));
        assert!(!css.contains("#E7E2D6"));
    }
}
