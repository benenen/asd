//! Main application component: sidebar + terminal pane, wired to the daemon
//! via a bidirectional eval bridge with ghostty-web.

use crate::bridge::{JsMessage, BRIDGE_JS};
use crate::conn::{self, Cmd, DaemonEvent};
use crate::model::Session;
use dioxus::prelude::*;
use tokio::sync::mpsc;

#[component]
pub fn App() -> Element {
    let mut sessions = use_signal(Vec::<Session>::new);
    let mut active_name = use_signal(|| None::<String>);
    let mut daemon_up = use_signal(|| false);
    let mut remote_input = use_signal(String::new);
    let mut status_text = use_signal(|| String::from("connecting\u{2026}"));

    let (daemon_cmd_tx, daemon_cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let tx_for_ui = daemon_cmd_tx.clone();
    let mut rx_opt = Some(daemon_cmd_rx);

    let mut sessions_w = sessions;
    let mut daemon_w = daemon_up;
    let mut status_w = status_text;
    let mut active_w = active_name;

    use_coroutine(move |_dummy: UnboundedReceiver<()>| {
        let cmd_rx = rx_opt.take().expect("coroutine restarted unexpectedly");
        let cmd_tx = daemon_cmd_tx.clone();
        async move {
            let desktop = dioxus::desktop::window();
            let mut eval = document::eval(BRIDGE_JS);

            let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<DaemonEvent>();

            tokio::spawn(async move {
                if let Err(e) = conn::run(ev_tx, cmd_rx).await {
                    tracing::error!("daemon: {e}");
                }
            });

            status_w.set("connecting\u{2026}".into());

            // Wait for the bridge to signal readiness before auto-attaching.
            let mut bridge_ready = false;
            let mut first_list = true;
            let mut pending_attach: Option<String> = None;

            loop {
                tokio::select! {
                    msg = eval.recv() => {
                        match msg {
                            Ok(val) => match serde_json::from_value::<JsMessage>(val) {
                                Ok(JsMessage::Status { msg }) => {
                                    tracing::info!("bridge: {msg}");
                                    if msg.contains("bridge ready") {
                                        bridge_ready = true;
                                        // Flush any pending attach.
                                        if let Some(name) = pending_attach.take() {
                                            tracing::info!("flushing pending attach to {name}");
                                            let _ = cmd_tx.send(Cmd::Attach {
                                                name: name.clone(),
                                                cols: 120, rows: 40,
                                            });
                                            active_w.set(Some(name));
                                        }
                                    }
                                }
                                Ok(JsMessage::Input { data }) => {
                                    let _ = cmd_tx.send(Cmd::Input { bytes: data.into_bytes() });
                                }
                                Ok(JsMessage::Resize { cols, rows }) => {
                                    let _ = cmd_tx.send(Cmd::Resize { cols, rows });
                                }
                                Err(e) => tracing::warn!("bridge unexpected msg: {e}"),
                            },
                            Err(e) => {
                                tracing::error!("eval recv: {e}");
                                break;
                            }
                        }
                    }
                    ev = ev_rx.recv() => {
                        let Some(ev) = ev else { break };
                        match ev {
                            DaemonEvent::Sessions(list) => {
                                let v: Vec<Session> = list.into_iter().map(Into::into).collect();
                                let count = v.len();
                                sessions_w.set(v.clone());
                                daemon_w.set(true);

                                if first_list {
                                    first_list = false;
                                    if count > 0 {
                                        let name = v[0].name.clone();
                                        status_w.set(format!("{count} session(s)"));
                                        if bridge_ready {
                                            tracing::info!("auto-attaching to {name}");
                                            let _ = cmd_tx.send(Cmd::Attach {
                                                name: name.clone(),
                                                cols: 120, rows: 40,
                                            });
                                            active_w.set(Some(name));
                                        } else {
                                            tracing::info!("deferring attach to {name} (bridge not ready)");
                                            pending_attach = Some(name);
                                        }
                                    } else if bridge_ready {
                                        let _ = cmd_tx.send(Cmd::Create);
                                        status_w.set("creating session\u{2026}".into());
                                    } else {
                                        pending_attach = None; // will auto-create after bridge ready
                                    }
                                }
                            }
                            DaemonEvent::Output(data) | DaemonEvent::Snapshot(data) => {
                                let json_str = serde_json::to_string(
                                    &String::from_utf8_lossy(&data)
                                ).unwrap_or_else(|_| "\"\"".into());
                                let script = format!(
                                    "window.__asdWrite&&window.__asdWrite(JSON.parse({}));",
                                    json_str
                                );
                                if let Err(e) = desktop.webview.evaluate_script(&script) {
                                    tracing::error!("evaluate_script: {e}");
                                }
                            }
                            DaemonEvent::Created { name } => {
                                active_w.set(Some(name.clone()));
                                let _ = cmd_tx.send(Cmd::Attach {
                                    name, cols: 120, rows: 40,
                                });
                                status_w.set("session created".into());
                            }
                            DaemonEvent::SessionEnded { name, msg } => {
                                if *active_w.read() == Some(name.clone()) {
                                    active_w.set(None);
                                }
                                tracing::info!("session {name}: {msg}");
                            }
                        }
                    }
                    else => break,
                }
            }
        }
    });

    let tagline = {
        let n = sessions.read().len();
        if n == 1 { format!("{n} session") } else { format!("{n} sessions") }
    };
    let show_active = active_name.read();

    rsx! {
        div { class: "app-container",
            div { class: "sidebar",
                div { class: "brand",
                    span { class: "logo", "asd" }
                    span { class: "tagline", "{tagline}" }
                }
                div { class: "section-label", "HOSTS" }
                div { class: "host-list",
                    div { class: "host-group",
                        div { class: "host-head",
                            span { class: "host-dot" }
                            span { "local" }
                        }
                        for session in sessions.read().iter() {
                            {
                                let session_name = session.name.clone();
                                let is_active = show_active.as_deref() == Some(&session_name);
                                let clients = session.attached_clients;
                                let cmd = session.command.clone();
                                let tx = tx_for_ui.clone();

                                rsx! {
                                    div {
                                        class: if is_active { "session-row active" } else { "session-row" },
                                        key: "{session_name}",
                                        onclick: move |_| {
                                            let _ = tx.send(Cmd::Attach {
                                                name: session_name.clone(),
                                                cols: 120, rows: 40,
                                            });
                                            active_name.set(Some(session_name.clone()));
                                        },
                                        span {
                                            class: if clients > 0 { "session-dot attached" } else { "session-dot" }
                                        }
                                        span { class: "session-name", "{session_name}" }
                                        span { class: "session-cmd", "{cmd}" }
                                    }
                                }
                            }
                        }
                    }
                    input {
                        class: "remote-input",
                        placeholder: "user@host  \u{21b5} connect",
                        value: "{remote_input.read()}",
                        oninput: move |e| remote_input.set(e.value()),
                    }
                }
                div { class: "sidebar-footer",
                    span {
                        span { class: "status-dot",
                            style { if daemon_up() { "background:#79D18C" } else { "background:#E5595E" } }
                        }
                        " {status_text.read()}"
                    }
                    button {
                        class: "settings-btn",
                        onclick: {
                            let tx = tx_for_ui.clone();
                            move |_| { let _ = tx.send(Cmd::Create); }
                        },
                        "+"
                    }
                }
            }
            div { class: "terminal-pane",
                div { class: "term-head",
                    span { "{show_active.as_deref().unwrap_or(\"select a session\")}" }
                }
                div { class: "term-body",
                    div { id: "terminal", class: "term-inner" }
                }
                div { class: "status-bar",
                    span { "asd terminal" }
                    button { class: "settings-btn", "\u{2699}" }
                }
            }
        }
    }
}
