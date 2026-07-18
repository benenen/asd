//! Handling of a single UDS connection (spec §4/§5).
//!
//! Split into two tasks rather than a single select loop: `read_frame` is
//! not cancel-safe (cancelling mid-frame tears the byte stream), so inbound
//! and outbound each get their own task, and every frame written to the
//! socket is serialized through the outbound queue.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use asd_proto::{Frame, FrameReader, FrameWriter, PROTO_VERSION, code};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::registry::Registry;
use crate::session::{ClientSink, ConnItem, SessionMsg, data_frame_size};

/// The client↔session association after attach.
struct Attached {
    session_tx: std::sync::mpsc::Sender<SessionMsg>,
    client_id: u64,
}

pub async fn handle_conn(stream: UnixStream, registry: Arc<Mutex<Registry>>, conn_id: u64) {
    let (r, w) = stream.into_split();
    let mut reader = FrameReader::new(r);
    let mut writer = FrameWriter::new(w);

    // ---- Handshake: the client sends Hello first ----
    match reader.read_frame().await {
        Ok(Some(Frame::Hello { proto_version, .. })) => {
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
    }

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
            Frame::Create { name, cmd } => match Registry::create(&registry, name, cmd) {
                Ok(name) => reply(Frame::Created { name }),
                Err((code, msg)) => reply(Frame::Error { code, msg }),
            },
            Frame::Kill { name } => {
                if let Err((code, msg)) = registry.lock().unwrap().kill(&name) {
                    reply(Frame::Error { code, msg });
                }
            }
            Frame::Rename { name, new_name } => {
                match registry.lock().unwrap().rename(&name, &new_name) {
                    Ok(()) => reply(Frame::Ack),
                    Err((code, msg)) => reply(Frame::Error { code, msg }),
                }
            }
            Frame::Attach { name, cols, rows } => {
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
                    .send(SessionMsg::Attach { sink, cols, rows })
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
                    let _ = a.session_tx.send(SessionMsg::Input(bytes));
                }
            }
            Frame::Resize { cols, rows } => {
                if let Some(a) = &attached {
                    let _ = a.session_tx.send(SessionMsg::Resize { cols, rows });
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
            Frame::SendInput { name, bytes } => match registry.lock().unwrap().get(&name) {
                Some(handle) => {
                    let _ = handle.tx.send(SessionMsg::Input(bytes));
                    reply(Frame::Ack);
                }
                None => reply(Frame::Error {
                    code: code::NO_SUCH_SESSION,
                    msg: format!("no such session '{name}'"),
                }),
            },
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
    let _ = out_tx.send(ConnItem::Close);
    let _ = write_task.await;
}
