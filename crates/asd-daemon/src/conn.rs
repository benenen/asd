//! Handling of a single UDS connection (spec §4/§5).
//!
//! Split into two tasks rather than a single select loop: `read_frame` is
//! not cancel-safe (cancelling mid-frame tears the byte stream), so inbound
//! and outbound each get their own task, and every frame written to the
//! socket is serialized through the outbound queue.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, PROTO_VERSION, code};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::registry::Registry;
use crate::session::{AttachClass, ClientSink, ConnItem, SessionMsg, data_frame_size};

/// The client↔session association after attach.
struct Attached {
    session_tx: std::sync::mpsc::Sender<SessionMsg>,
    client_id: u64,
}

/// Longest status line a session may set, in bytes. Long enough for a sentence
/// about what a program is doing, short enough that it costs nothing to carry
/// in every session list.
const MAX_STATUS_LINE: usize = 512;

/// Cut `s` to at most `max` bytes without splitting a character.
fn truncate_on_char_boundary(mut s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s
}

pub async fn handle_conn(
    r: impl AsyncRead + Unpin + Send + 'static,
    w: impl AsyncWrite + Unpin + Send + 'static,
    registry: Arc<Mutex<Registry>>,
    conn_id: u64,
) {
    let mut reader = FrameReader::new(r);
    let mut writer = FrameWriter::new(w);

    // ---- Handshake: the client sends Hello first ----
    let client_kind = match reader.read_frame().await {
        Ok(Some(Frame::Hello {
            proto_version,
            kind,
        })) => {
            if proto_version != PROTO_VERSION {
                // Contract: version mismatch → Error{code=1} then disconnect
                let _ = writer
                    .write_frame(&Frame::Error {
                        code: code::VERSION_MISMATCH,
                        msg: format!(
                            "proto version mismatch: daemon={PROTO_VERSION} client={proto_version}"
                        ),
                    })
                    .await;
                return;
            }
            if writer
                .write_frame(&Frame::HelloAck {
                    proto_version: PROTO_VERSION,
                    daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                })
                .await
                .is_err()
            {
                return;
            }
            kind
        }
        Ok(Some(_)) => {
            let _ = writer
                .write_frame(&Frame::Error {
                    code: code::BAD_HANDSHAKE,
                    msg: "expected Hello as first frame".into(),
                })
                .await;
            return;
        }
        _ => return,
    };

    // ---- Outbound queue: the sole channel for all frames written on this connection ----
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ConnItem>();
    let queued = Arc::new(AtomicUsize::new(0));

    let write_task = {
        let queued = Arc::clone(&queued);
        tokio::spawn(async move {
            while let Some(item) = out_rx.recv().await {
                match item {
                    ConnItem::Frame(frame) => {
                        let sz = data_frame_size(&frame);
                        let res = writer.write_frame(&frame).await;
                        queued.fetch_sub(sz, Ordering::Relaxed);
                        if res.is_err() {
                            break;
                        }
                    }
                    ConnItem::Close => break,
                }
            }
            // The writer drops as the task ends → half-closes the write side,
            // and the read end sees EOF shortly after
        })
    };

    // ---- Inbound loop ----
    let mut attached: Option<Attached> = None;
    // The session this connection follows, if any (v9). Separate from
    // `attached`: a connection may do either, and they mean different things to
    // the session thread.
    let mut following: Option<Attached> = None;
    loop {
        let frame = match reader.read_frame().await {
            Ok(Some(f)) => f,
            Ok(None) => break, // client disconnected normally
            Err(e) => {
                debug!(conn = conn_id, error = %e, "read error, closing");
                break;
            }
        };
        // Control-plane replies go straight to the outbound queue (no
        // data-plane quota)
        let reply = |f: Frame| {
            let _ = out_tx.send(ConnItem::Frame(f));
        };

        match frame {
            Frame::ListSessions => {
                reply(Frame::SessionList {
                    sessions: registry.lock().unwrap().list(),
                });
            }
            Frame::HostMetrics => {
                // Read the sampler's stored reading. Nothing is measured here:
                // see `crate::metrics`.
                reply(Frame::HostMetricsReply {
                    sample: registry.lock().unwrap().host_metrics(),
                });
            }
            Frame::Create { name, cmd, cwd } => {
                let cwd = cwd.map(std::path::PathBuf::from);
                match Registry::create(&registry, name, cmd, cwd) {
                    Ok(name) => reply(Frame::Created { name }),
                    Err((code, msg)) => reply(Frame::Error { code, msg }),
                }
            }
            Frame::Kill { name } => {
                if let Err((code, msg)) = registry.lock().unwrap().kill(&name) {
                    reply(Frame::Error { code, msg });
                }
            }
            // Set from inside the session it names, in the normal case: the
            // child has `$ASD_SESSION` and `$ASD_SOCKET`, so `asd status` finds
            // its own session and this daemon without being told either.
            Frame::SetStatusLine { name, line } => {
                // Every `list` carries this to every client, and the TUI polls
                // the list every 1.5s, so an unbounded line would be a way for
                // one session to tax the whole daemon. Keep the first
                // `MAX_STATUS_LINE` bytes and drop the rest.
                let line = truncate_on_char_boundary(line, MAX_STATUS_LINE);
                match registry.lock().unwrap().get(&name) {
                    Some(handle) => {
                        if let Ok(mut current) = handle.meta.status_line.lock() {
                            *current = line;
                        }
                        reply(Frame::Ack);
                    }
                    None => reply(Frame::Error {
                        code: code::NO_SUCH_SESSION,
                        msg: format!("no such session '{name}'"),
                    }),
                }
            }
            Frame::Rename { name, new_name } => {
                match registry.lock().unwrap().rename(&name, &new_name) {
                    Ok(()) => reply(Frame::Ack),
                    Err((code, msg)) => reply(Frame::Error { code, msg }),
                }
            }
            Frame::Attach {
                name,
                cols,
                rows,
                view_id,
                appearance,
                read_only,
            } => {
                if client_kind == ClientKind::Tui && view_id == 0 {
                    reply(Frame::Error {
                        code: code::BAD_HANDSHAKE,
                        msg: "TUI Attach requires a nonzero view_id".to_string(),
                    });
                    continue;
                }
                // Attaching supersedes any prior attachment on this connection.
                // A session that dies while attached cannot clear this
                // read-side bookkeeping — the session thread only reaches the
                // outbound sink (§session.rs endpoint) — so a leftover
                // `attached` can point at an already-dead session. Rejecting the
                // next Attach as ALREADY_ATTACHED would then wedge the
                // connection: the client's pane stays blank until it reconnects
                // (the asd-tui "blank after kill-then-new-session" bug). Release
                // the old attachment first; a Detach to a dead session thread is
                // harmlessly dropped.
                if let Some(a) = attached.take() {
                    let _ = a.session_tx.send(SessionMsg::Detach {
                        client_id: a.client_id,
                    });
                }
                let Some(handle) = registry.lock().unwrap().get(&name) else {
                    reply(Frame::Error {
                        code: code::NO_SUCH_SESSION,
                        msg: format!("no such session '{name}'"),
                    });
                    continue;
                };
                let sink = ClientSink::new(conn_id, out_tx.clone(), Arc::clone(&queued));
                if handle
                    .tx
                    .send(SessionMsg::Attach {
                        sink,
                        class: if client_kind == ClientKind::Tui {
                            AttachClass::ExclusiveTui
                        } else {
                            AttachClass::Shared
                        },
                        requested_name: name.clone(),
                        view_id,
                        cols,
                        rows,
                        appearance,
                        read_only,
                    })
                    .is_err()
                {
                    reply(Frame::Error {
                        code: code::SESSION_EXITED,
                        msg: format!("session '{name}' exited"),
                    });
                    continue;
                }
                attached = Some(Attached {
                    session_tx: handle.tx.clone(),
                    client_id: conn_id,
                });
            }
            Frame::Input { bytes } => {
                if let Some(a) = &attached {
                    let _ = a.session_tx.send(SessionMsg::Input {
                        client_id: a.client_id,
                        bytes,
                    });
                }
            }
            Frame::Resize { cols, rows } => {
                if let Some(a) = &attached {
                    let _ = a.session_tx.send(SessionMsg::Resize {
                        client_id: a.client_id,
                        cols,
                        rows,
                    });
                }
            }
            Frame::Detach => {
                if let Some(a) = attached.take() {
                    let _ = a.session_tx.send(SessionMsg::Detach {
                        client_id: a.client_id,
                    });
                }
            }
            Frame::FetchHistory { start, count } => {
                if let Some(a) = &attached {
                    // Same out_tx/queued as the attach sink so the History
                    // reply rides the connection's ordered outbound queue.
                    let sink = ClientSink::new(conn_id, out_tx.clone(), Arc::clone(&queued));
                    let _ = a
                        .session_tx
                        .send(SessionMsg::FetchHistory { sink, start, count });
                } else {
                    reply(Frame::Error {
                        code: code::BAD_HANDSHAKE,
                        msg: "FetchHistory before Attach".into(),
                    });
                }
            }
            Frame::Refresh => {
                if let Some(a) = &attached {
                    let sink = ClientSink::new(conn_id, out_tx.clone(), Arc::clone(&queued));
                    let _ = a.session_tx.send(SessionMsg::Refresh { sink });
                } else {
                    reply(Frame::Error {
                        code: code::BAD_HANDSHAKE,
                        msg: "Refresh before Attach".into(),
                    });
                }
            }
            // Scripting (v4): name-addressed, attach-free — the connection's
            // `attached` state is untouched.
            Frame::SendInput { name, bytes, enter } => {
                let handle = registry.lock().unwrap().get(&name);
                match handle {
                    Some(handle) => {
                        let (completed, applied) = tokio::sync::oneshot::channel();
                        if handle
                            .tx
                            .send(SessionMsg::ScriptInput {
                                bytes,
                                enter,
                                completed,
                            })
                            .is_err()
                        {
                            reply(Frame::Error {
                                code: code::SESSION_EXITED,
                                msg: format!("session '{name}' exited before input"),
                            });
                        } else {
                            match applied.await {
                                Ok(Ok(())) => reply(Frame::Ack),
                                Ok(Err(error)) => reply(Frame::Error {
                                    code: code::SESSION_EXITED,
                                    msg: format!("session '{name}' input failed: {error}"),
                                }),
                                Err(_) => reply(Frame::Error {
                                    code: code::SESSION_EXITED,
                                    msg: format!("session '{name}' exited during input"),
                                }),
                            }
                        }
                    }
                    None => reply(Frame::Error {
                        code: code::NO_SUCH_SESSION,
                        msg: format!("no such session '{name}'"),
                    }),
                }
            }
            // Following is a subscription, not an attachment: it shares the
            // connection sink with the one-shot scripting frames, but the
            // session keeps it out of its client list. Tracked here only so
            // losing the connection unsubscribes it, the way it detaches.
            Frame::Follow { name } => match registry.lock().unwrap().get(&name) {
                Some(handle) => {
                    let sink = ClientSink::new(conn_id, out_tx.clone(), Arc::clone(&queued));
                    if handle.tx.send(SessionMsg::Follow { sink }).is_ok() {
                        following = Some(Attached {
                            session_tx: handle.tx.clone(),
                            client_id: conn_id,
                        });
                    }
                }
                None => reply(Frame::Error {
                    code: code::NO_SUCH_SESSION,
                    msg: format!("no such session '{name}'"),
                }),
            },
            Frame::Unfollow { name } => {
                let _ = &name;
                if let Some(f) = following.take() {
                    let _ = f.session_tx.send(SessionMsg::Unfollow {
                        client_id: f.client_id,
                    });
                }
            }
            Frame::Peek { name, scrollback } => match registry.lock().unwrap().get(&name) {
                Some(handle) => {
                    let sink = ClientSink::new(conn_id, out_tx.clone(), Arc::clone(&queued));
                    let _ = handle.tx.send(SessionMsg::Peek { sink, scrollback });
                }
                None => reply(Frame::Error {
                    code: code::NO_SUCH_SESSION,
                    msg: format!("no such session '{name}'"),
                }),
            },
            Frame::Inspect { name } => match registry.lock().unwrap().get(&name) {
                Some(handle) => {
                    // Metadata is gathered here; the session thread adds VT state.
                    let info = handle.info();
                    let sink = ClientSink::new(conn_id, out_tx.clone(), Arc::clone(&queued));
                    let _ = handle.tx.send(SessionMsg::Inspect { sink, info });
                }
                None => reply(Frame::Error {
                    code: code::NO_SUCH_SESSION,
                    msg: format!("no such session '{name}'"),
                }),
            },
            other => {
                warn!(conn = conn_id, frame = ?other, "unexpected frame from client");
                reply(Frame::Error {
                    code: code::BAD_HANDSHAKE,
                    msg: "unexpected frame".into(),
                });
            }
        }
    }

    // Connection loss means detach (spec §5: no explicit state)
    if let Some(a) = attached.take() {
        let _ = a.session_tx.send(SessionMsg::Detach {
            client_id: a.client_id,
        });
    }
    // ...and unfollow, for the same reason: the session would otherwise keep a
    // dead sink until its next output batch swept it up.
    if let Some(f) = following.take() {
        let _ = f.session_tx.send(SessionMsg::Unfollow {
            client_id: f.client_id,
        });
    }
    let _ = out_tx.send(ConnItem::Close);
    let _ = write_task.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_line_is_cut_without_splitting_a_character() {
        assert_eq!(truncate_on_char_boundary("abc".into(), 512), "abc");
        assert_eq!(truncate_on_char_boundary("abcdef".into(), 3), "abc");
        // Three bytes each: cutting at 4 must not leave half a character.
        assert_eq!(truncate_on_char_boundary("中文标题".into(), 4), "中");
        assert_eq!(truncate_on_char_boundary("中文标题".into(), 6), "中文");
    }
}
