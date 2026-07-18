//! Main application component: host-grouped session sidebar + ghostty-web
//! terminal pane + status bar, plus the settings overlay (saved SSH
//! connections) — the M2 two-pane layout (spec §7, boo `boo ui` parity).
//!
//! Architecture: UI handlers send [`AppCmd`]s to the supervisor loop inside
//! the app coroutine. The supervisor spawns one [`conn::run_host`] task per
//! host, routes commands to the right actor, and folds every [`UiEvent`] into
//! the Dioxus signals. PTY bytes go straight to ghostty-web through
//! `window.__asdWrite`, with `window.__asdReset` before each Snapshot so no
//! old content survives a session switch.

use std::collections::HashMap;

use dioxus::prelude::*;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::bridge::{BRIDGE_JS, JsMessage};
use crate::conn::{self, HostCmd, HostHandle, UiEvent};
use crate::model::{
    HostId, HostKind, HostState, LOCAL_ID, Model, RemoteSpec, is_host_key_issue, short_age,
    short_cmd, short_reason,
};
use crate::settings::{AuthKind, SettingsConfig, SettingsPage, SshConnection, SshForm};

/// Commands the UI sends to the supervisor loop.
#[derive(Debug, Clone)]
pub enum AppCmd {
    AddRemote {
        id: HostId,
        spec: RemoteSpec,
    },
    RemoveHost {
        id: HostId,
    },
    /// Respawn the actor of a host that went down (its previous task exited).
    Reconnect {
        id: HostId,
    },
    SetActive {
        host: HostId,
        name: String,
        cols: u16,
        rows: u16,
    },
    Input(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    Create {
        host: HostId,
    },
    Kill {
        host: HostId,
        name: String,
    },
    Rename {
        host: HostId,
        name: String,
        new_name: String,
    },
}

/// State of the active session's stream, shown in the terminal header.
#[derive(Debug, Clone, PartialEq)]
enum Status {
    Live,
    Ended(String),
    Disconnected(String),
}

/// Inline session-rename state: which session (`host` + its current `old` name)
/// is being edited, the edited `text`, and the last validation error.
#[derive(Debug, Clone, PartialEq)]
struct Rename {
    host: HostId,
    old: String,
    text: String,
    error: Option<String>,
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A dedicated tokio runtime for the host actors, isolated from the Dioxus
/// desktop embedded runtime (an 0.8-alpha moving target). Channel sends from
/// these workers wake the UI coroutine reliably (the scheduler waker posts to
/// the event-loop proxy), so events fold into signals promptly.
fn bg() -> tokio::runtime::Handle {
    use std::sync::OnceLock;
    static BG: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    BG.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("asd-conn")
            .enable_all()
            .build()
            .expect("bg runtime")
    })
    .handle()
    .clone()
}

#[component]
pub fn App() -> Element {
    let mut model = use_signal(Model::with_local);
    let mut status = use_signal(|| Status::Live);
    let saved_ssh = use_signal(|| SettingsConfig::load().ssh_connections);
    let mut connect_menu_open = use_signal(|| false);
    let mut settings_open = use_signal(|| false);
    let mut settings_page = use_signal(|| SettingsPage::General);
    let form = use_signal(|| None::<SshForm>);
    // Last grid reported by ghostty-web; used to size Attach requests.
    let grid = use_signal(|| (120u16, 40u16));
    // Destructive actions confirm before acting (both were one-click before):
    // killing a session pops a confirm overlay; deleting a saved connection
    // shows an inline confirm on that row (`Some(index)`).
    let mut confirm_kill = use_signal(|| None::<(HostId, String)>);
    // Which saved connection (by stable id) is mid inline-delete confirm.
    let confirm_delete = use_signal(|| None::<u64>);
    // The session row (if any) whose name is being edited inline.
    let rename = use_signal(|| None::<Rename>);

    // The command channel must be created ONCE and cached across renders
    // (use_hook): a per-render channel drops the previous sender on the first
    // re-render, the supervisor's `app_rx.recv()` yields `None`, and the whole
    // supervisor — host actors, eval bridge — silently unwinds.
    let (tx, rx_slot) = use_hook(|| {
        let (tx, rx) = mpsc::unbounded_channel::<AppCmd>();
        (tx, std::rc::Rc::new(std::cell::RefCell::new(Some(rx))))
    });

    let model_w = model;
    let status_w = status;
    let grid_w = grid;

    use_coroutine(move |_dummy: UnboundedReceiver<()>| {
        let app_rx = rx_slot
            .borrow_mut()
            .take()
            .expect("coroutine restarted unexpectedly");
        async move {
            supervisor(app_rx, model_w, status_w, grid_w).await;
        }
    });

    // ── UI helpers (closures over the signals + command sender) ──────

    let select_session = {
        let tx = tx.clone();
        move |host: HostId, name: String| {
            // Re-selecting the live active session is a no-op, but when its
            // stream ended (killed/exited — possibly recreated under the same
            // name) a click must re-attach.
            if model.read().is_active(host, &name) && *status.read() == Status::Live {
                return;
            }
            let (cols, rows) = *grid.read();
            model.write().select(host, name.clone());
            status.set(Status::Live);
            reset_terminal();
            let _ = tx.send(AppCmd::SetActive {
                host,
                name,
                cols,
                rows,
            });
        }
    };

    let add_saved = {
        let tx = tx.clone();
        move |conn: SshConnection| {
            let spec = RemoteSpec {
                conn_id: conn.id,
                user: conn.user.clone(),
                host: conn.host.clone(),
                port: conn.port,
                auth: conn.auth.clone(),
                name: conn.name.clone(),
            };
            if model.read().has_connection(conn.id) {
                return;
            }
            let id = model.write().add_remote(spec.clone());
            let _ = tx.send(AppCmd::AddRemote { id, spec });
            connect_menu_open.set(false);
        }
    };

    let save_config = move |list: &[SshConnection]| {
        SettingsConfig {
            ssh_connections: list.to_vec(),
        }
        .save();
    };

    // ── view ──────────────────────────────────────────────────────────

    let m = model.read();
    let total = m.total_sessions();
    let host_count = m.hosts.len();
    let pool = format!(
        "{total} session{} · {host_count} host{}",
        if total == 1 { "" } else { "s" },
        if host_count == 1 { "" } else { "s" }
    );
    let active = m.active.clone();
    let active_label = active.as_ref().map(|(h, n)| {
        let tag = m
            .host(*h)
            .map(|host| host.label().to_uppercase())
            .unwrap_or_default();
        (n.clone(), tag, m.host(*h).is_some_and(|h| h.is_remote()))
    });
    let (gc, gr) = *grid.read();
    let local_up = m.host(LOCAL_ID).is_some_and(|h| h.state == HostState::Up);
    let hosts_view = m.hosts.clone();
    drop(m);

    let now = now_ms();
    let saved_list = saved_ssh.read().clone();
    let menu_open = *connect_menu_open.read();

    rsx! {
        // Head content via the dedicated document:: API. Both are inlined
        // (include_str! / OUT_DIR) so the shipped `asd` stays one
        // self-contained binary — `asset!()` files would have to travel next
        // to the executable. The vendor <script> lands before the bridge eval
        // needs it; the bridge polls for `window.GhosttyWeb` regardless.
        document::Script { {crate::VENDOR_JS} }
        document::Style { {crate::APP_CSS} }
        div { class: "app-container",
            // ── sidebar ─────────────────────────────────────────────
            div { class: "sidebar",
                div { class: "brand",
                    div { class: "brand-badges",
                        span { class: "badge", "a" }
                        span { class: "badge", "s" }
                        span { class: "badge", "d" }
                        span { class: "brand-meta",
                            span { class: "brand-title", "session pool" }
                            span { class: "brand-sub", "{pool}" }
                        }
                    }
                }
                div { class: "hosts-header",
                    span { class: "section-label", "HOSTS" }
                    button {
                        class: "icon-btn",
                        title: "add a saved SSH connection",
                        onclick: move |_| {
                            let v = *connect_menu_open.read();
                            connect_menu_open.set(!v);
                        },
                        "+"
                    }
                }
                if menu_open {
                    div { class: "connect-menu",
                        if saved_list.is_empty() {
                            div { class: "connect-empty", "No saved connections yet." }
                        }
                        for conn in saved_list.iter() {
                            {
                                let added = model.read().has_connection(conn.id);
                                let conn2 = conn.clone();
                                let mut add = add_saved.clone();
                                rsx! {
                                    div {
                                        key: "conn-{conn.id}",
                                        class: if added { "connect-row added" } else { "connect-row" },
                                        onclick: move |_| {
                                            if !added { add(conn2.clone()); }
                                        },
                                        div { class: "connect-name", "{conn.name}" }
                                        div { class: "connect-sub", "{conn.label()}" }
                                        if added {
                                            span { class: "connect-tag", "added" }
                                        }
                                    }
                                }
                            }
                        }
                        div {
                            class: "connect-manage",
                            onclick: move |_| {
                                connect_menu_open.set(false);
                                settings_page.set(SettingsPage::Connections);
                                settings_open.set(true);
                            },
                            "+ Manage connections…"
                        }
                    }
                }
                div { class: "host-list",
                    for host in hosts_view.iter() {
                        {host_group(host, &active, now, select_session.clone(), tx.clone(), model, status, confirm_kill, rename)}
                    }
                }
                div { class: "sidebar-footer",
                    if local_up {
                        span {
                            span { class: "status-dot up" }
                            " daemon up · proto v{asd_proto::PROTO_VERSION}"
                        }
                    } else {
                        span {
                            class: "reconnect",
                            onclick: {
                                let tx = tx.clone();
                                move |_| {
                                    model.write().set_state(LOCAL_ID, HostState::Connecting);
                                    let _ = tx.send(AppCmd::Reconnect { id: LOCAL_ID });
                                }
                            },
                            span { class: "status-dot down" }
                            " daemon down · click to reconnect"
                        }
                    }
                }
            }

            // ── terminal pane ───────────────────────────────────────
            div { class: "terminal-pane",
                div { class: "term-head",
                    if let Some((name, tag, remote)) = active_label {
                        span { class: "term-title", "{name}" }
                        span {
                            class: if remote { "host-tag remote" } else { "host-tag" },
                            "{tag}"
                        }
                        {match status.read().clone() {
                            Status::Live => rsx! {},
                            Status::Ended(msg) => rsx! {
                                span { class: "term-note", "ended — {msg}" }
                            },
                            Status::Disconnected(msg) => rsx! {
                                span { class: "term-note", "disconnected — {msg}" }
                            },
                        }}
                        span { class: "term-size", "{gc} × {gr}" }
                    } else {
                        span { class: "term-hint", "select a session" }
                    }
                }
                div { class: "term-body",
                    div { id: "terminal", class: "term-inner" }
                }
                div { class: "status-bar",
                    span {
                        if let Some((h, n)) = &active {
                            b { "{n}" }
                            span { class: "dim", " attached" }
                            {
                                let _ = h;
                            }
                        } else {
                            span { class: "dim", "no session" }
                        }
                    }
                    span { class: "status-actions",
                        button {
                            class: "bar-btn",
                            title: "new session on the active host",
                            onclick: {
                                let tx = tx.clone();
                                move |_| {
                                    let host = model
                                        .read()
                                        .active
                                        .as_ref()
                                        .map(|(h, _)| *h)
                                        .unwrap_or(LOCAL_ID);
                                    let _ = tx.send(AppCmd::Create { host });
                                }
                            },
                            "+ new"
                        }
                        button {
                            class: "bar-btn",
                            title: "kill the active session",
                            onclick: move |_| {
                                if let Some((host, name)) = model.read().active.clone() {
                                    confirm_kill.set(Some((host, name)));
                                }
                            },
                            "× kill"
                        }
                        button {
                            class: "bar-btn",
                            title: "settings",
                            onclick: move |_| {
                                settings_page.set(SettingsPage::General);
                                settings_open.set(true);
                            },
                            "⚙"
                        }
                    }
                }
            }

            // ── settings overlay ────────────────────────────────────
            if *settings_open.read() {
                {settings_view(settings_page, form, saved_ssh, model, tx.clone(), settings_open, save_config, confirm_delete)}
            }

            // ── kill confirmation overlay ───────────────────────────
            if let Some((host, name)) = confirm_kill.read().clone() {
                {
                    let name_btn = name.clone();
                    let tx = tx.clone();
                    rsx! {
                        div { class: "confirm-overlay",
                            div { class: "confirm-card",
                                div { class: "confirm-title", "Kill session \"{name}\"?" }
                                div { class: "confirm-msg",
                                    "The session and its processes are terminated (SIGHUP). This can't be undone."
                                }
                                div { class: "confirm-actions",
                                    button {
                                        class: "bar-btn",
                                        onclick: move |_| confirm_kill.set(None),
                                        "Cancel"
                                    }
                                    button {
                                        class: "bar-btn danger",
                                        onclick: move |_| {
                                            let _ = tx.send(AppCmd::Kill { host, name: name_btn.clone() });
                                            confirm_kill.set(None);
                                        },
                                        "Kill"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Reset the ghostty-web terminal (blank it) via the direct JS entry point.
fn reset_terminal() {
    let _ = dioxus::desktop::window()
        .webview
        .evaluate_script("window.__asdReset&&window.__asdReset();");
}

/// One host group in the sidebar: header (rail + dot + label + actions) and
/// its session rows.
#[allow(clippy::too_many_arguments)]
fn host_group(
    host: &crate::model::Host,
    active: &Option<(HostId, String)>,
    now: u64,
    select: impl FnMut(HostId, String) + Clone + 'static,
    tx: UnboundedSender<AppCmd>,
    mut model: Signal<Model>,
    _status: Signal<Status>,
    mut confirm_kill: Signal<Option<(HostId, String)>>,
    mut rename: Signal<Option<Rename>>,
) -> Element {
    let id = host.id;
    // Sibling names on this host, for the inline rename's dup check.
    let sibling_names: Vec<String> = host.sessions.iter().map(|s| s.name.clone()).collect();
    let remote = host.is_remote();
    let label = host.label();
    let sub = host.sublabel();
    let state = host.state.clone();
    let n = host.sessions.len();
    let rail = if remote { "rail remote" } else { "rail" };
    let dot = match state {
        HostState::Up => "host-dot up",
        HostState::Connecting => "host-dot connecting",
        HostState::Down(_) => "host-dot down",
    };
    // The truncated reason for the line plus the full text for the tooltip.
    let down = match &state {
        HostState::Down(msg) => Some((short_reason(msg), msg.clone())),
        _ => None,
    };
    let is_down = down.is_some();
    // When a remote is down on a host-key problem, offer to trust the key.
    let trust_spec = match (&down, &host.kind) {
        (Some((_, full)), HostKind::Ssh(spec)) if is_host_key_issue(full) => Some(spec.clone()),
        _ => None,
    };

    rsx! {
        div { class: "host-group", key: "host-{id}",
            div { class: "host-head",
                span { class: "{rail}" }
                span {
                    class: "{dot}",
                    // A down host reconnects on click (same actor respawn as
                    // the local footer link).
                    onclick: {
                        let tx = tx.clone();
                        move |_| {
                            if is_down {
                                model.write().set_state(id, HostState::Connecting);
                                let _ = tx.send(AppCmd::Reconnect { id });
                            }
                        }
                    },
                }
                span { class: "host-label", "{label}" }
                span { class: "host-sub", "{sub}" }
                span { class: "host-count", "{n}" }
                button {
                    class: "icon-btn",
                    title: "new session on {label}",
                    onclick: {
                        let tx = tx.clone();
                        move |_| { let _ = tx.send(AppCmd::Create { host: id }); }
                    },
                    "+"
                }
                if remote {
                    button {
                        class: "icon-btn",
                        title: "remove this host",
                        onclick: {
                            let tx = tx.clone();
                            move |_| {
                                model.write().remove_host(id);
                                let _ = tx.send(AppCmd::RemoveHost { id });
                            }
                        },
                        "×"
                    }
                }
            }
            if let Some((short, full)) = down {
                div {
                    class: "host-reason",
                    // Hover shows the full (untruncated) reason; the whole line
                    // is a reconnect affordance, not just the tiny status dot.
                    title: "{full} — click to reconnect",
                    onclick: {
                        let tx = tx.clone();
                        move |_| {
                            model.write().set_state(id, HostState::Connecting);
                            let _ = tx.send(AppCmd::Reconnect { id });
                        }
                    },
                    "{short} · reconnect"
                }
                if let Some(spec) = trust_spec {
                    button {
                        class: "trust-btn",
                        title: "add this host's key to ~/.ssh/known_hosts, then reconnect",
                        onclick: {
                            let tx = tx.clone();
                            let spec = spec.clone();
                            move |_| {
                                model.write().set_state(id, HostState::Connecting);
                                let tx = tx.clone();
                                let spec = spec.clone();
                                bg().spawn(async move {
                                    // Record the key (best-effort); reconnect
                                    // either way — success verifies, failure
                                    // re-surfaces the reason.
                                    let _ = crate::ssh::trust_host_key(&spec).await;
                                    let _ = tx.send(AppCmd::Reconnect { id });
                                });
                            }
                        },
                        "Trust host key"
                    }
                }
            }
            for s in host.sessions.iter() {
                {
                    let name = s.name.clone();
                    let is_active = active
                        .as_ref()
                        .is_some_and(|(h, sn)| *h == id && sn == &name);
                    let cmd = short_cmd(&s.command);
                    let age = short_age(s.created_ms, now);
                    let attached = s.attached_clients > 0;
                    let mut select = select.clone();
                    let name_click = name.clone();
                    let name_kill = name.clone();
                    let name_dbl = name.clone();
                    let name_kbd = name.clone();
                    let siblings = sibling_names.clone();
                    let tx_rename = tx.clone();
                    // This row's inline-rename state, if it is the one being edited.
                    let renaming = rename
                        .read()
                        .as_ref()
                        .filter(|r| r.host == id && r.old == name)
                        .cloned();
                    let row = if is_active { "session-row active" } else { "session-row" };
                    let sdot = if attached { "session-dot attached" } else { "session-dot" };
                    let sdot = if remote { format!("{sdot} remote") } else { sdot.to_string() };
                    rsx! {
                        div {
                            class: "{row}",
                            key: "s-{id}-{name}",
                            onclick: move |_| select(id, name_click.clone()),
                            span { class: "{sdot}" }
                            if let Some(rn) = renaming {
                                input {
                                    class: "session-rename",
                                    value: "{rn.text}",
                                    autofocus: true,
                                    onclick: move |e| e.stop_propagation(),
                                    oninput: move |e| {
                                        if let Some(r) = rename.write().as_mut() {
                                            r.text = e.value();
                                            r.error = None;
                                        }
                                    },
                                    onkeydown: move |e| match e.key() {
                                        Key::Enter => {
                                            let new = rename
                                                .read()
                                                .as_ref()
                                                .map(|r| r.text.clone())
                                                .unwrap_or_default();
                                            match crate::model::validate_rename(&new, &siblings, &name_kbd) {
                                                Ok(()) => {
                                                    let newt = new.trim().to_string();
                                                    if newt != name_kbd {
                                                        // Optimistic: update locally now; the
                                                        // list poll confirms (or reverts).
                                                        model.write().rename_session(id, &name_kbd, &newt);
                                                        let _ = tx_rename.send(AppCmd::Rename {
                                                            host: id,
                                                            name: name_kbd.clone(),
                                                            new_name: newt,
                                                        });
                                                    }
                                                    rename.set(None);
                                                }
                                                Err(msg) => {
                                                    if let Some(r) = rename.write().as_mut() {
                                                        r.error = Some(msg);
                                                    }
                                                }
                                            }
                                        }
                                        Key::Escape => rename.set(None),
                                        _ => {}
                                    },
                                }
                                if let Some(err) = rn.error {
                                    span { class: "rename-error", title: "{err}", "!" }
                                }
                            } else {
                                span {
                                    class: "session-name",
                                    title: "double-click to rename",
                                    ondoubleclick: move |e| {
                                        e.stop_propagation();
                                        rename.set(Some(Rename {
                                            host: id,
                                            old: name_dbl.clone(),
                                            text: name_dbl.clone(),
                                            error: None,
                                        }));
                                    },
                                    "{name}"
                                }
                            }
                            span { class: "session-cmd", "{cmd}" }
                            span { class: "session-age", "{age}" }
                            button {
                                class: "icon-btn kill",
                                title: "kill {name}",
                                onclick: move |e| {
                                    e.stop_propagation();
                                    confirm_kill.set(Some((id, name_kill.clone())));
                                },
                                "×"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The settings overlay: nav (General / Connections) + content page.
#[allow(clippy::too_many_arguments)]
fn settings_view(
    mut page: Signal<SettingsPage>,
    mut form: Signal<Option<SshForm>>,
    saved: Signal<Vec<SshConnection>>,
    model: Signal<Model>,
    _tx: UnboundedSender<AppCmd>,
    mut open: Signal<bool>,
    save_config: impl Fn(&[SshConnection]) + Clone + 'static,
    mut confirm_delete: Signal<Option<u64>>,
) -> Element {
    let cur = *page.read();
    let nav_item = |p: SettingsPage, label: &'static str| {
        let is_active = cur == p;
        rsx! {
            div {
                class: if is_active { "nav-item active" } else { "nav-item" },
                onclick: move |_| {
                    page.set(p);
                    form.set(None);
                    confirm_delete.set(None);
                },
                span { class: "nav-rail" }
                span { "{label}" }
            }
        }
    };

    rsx! {
        div { class: "settings-overlay",
            div { class: "settings-panel",
                div { class: "settings-nav",
                    div { class: "settings-title", "Settings" }
                    {nav_item(SettingsPage::General, "General")}
                    {nav_item(SettingsPage::Connections, "Connections")}
                    div { class: "nav-spacer" }
                    button {
                        class: "bar-btn",
                        onclick: move |_| {
                            open.set(false);
                            form.set(None);
                            confirm_delete.set(None);
                        },
                        "Close"
                    }
                }
                div { class: "settings-content",
                    match cur {
                        SettingsPage::General => rsx! {
                            div { class: "page-title", "General" }
                            div { class: "setting-row",
                                span { class: "setting-key", "App" }
                                span { "asd GPU Terminal Client" }
                            }
                            div { class: "setting-row",
                                span { class: "setting-key", "Version" }
                                span { {env!("CARGO_PKG_VERSION")} }
                            }
                            div { class: "setting-row",
                                span { class: "setting-key", "Protocol" }
                                span { "v{asd_proto::PROTO_VERSION}" }
                            }
                        },
                        SettingsPage::Connections => {
                            connections_page(form, saved, model, save_config.clone(), confirm_delete)
                        }
                    }
                }
            }
        }
    }
}

/// The Connections page: saved-connection list with edit/delete, or the
/// add/edit form when one is open.
fn connections_page(
    mut form: Signal<Option<SshForm>>,
    mut saved: Signal<Vec<SshConnection>>,
    model: Signal<Model>,
    save_config: impl Fn(&[SshConnection]) + Clone + 'static,
    mut confirm_delete: Signal<Option<u64>>,
) -> Element {
    if let Some(f) = form.read().clone() {
        return connection_form(f, form, saved, save_config);
    }
    let list = saved.read().clone();
    rsx! {
        div { class: "page-head",
            div { class: "page-title accent", "SSH Connections" }
            button {
                class: "bar-btn accent",
                onclick: move |_| form.set(Some(SshForm::default())),
                "+ Add"
            }
        }
        if list.is_empty() {
            div { class: "conn-empty",
                "No connections yet. Add one to reach a remote asd daemon over SSH."
            }
        }
        for c in list.iter() {
            {
                let added = model.read().has_connection(c.id);
                let edit = c.clone();
                let cid = c.id;
                let save_del = save_config.clone();
                rsx! {
                    div { class: "conn-row", key: "saved-{cid}",
                        div { class: "conn-main",
                            div { class: "conn-name", "{c.name}" }
                            div { class: "conn-sub", "{c.label()}" }
                        }
                        span { class: "conn-auth", "{c.auth.tag()}" }
                        if added {
                            span { class: "connect-tag", "added" }
                        }
                        if *confirm_delete.read() == Some(cid) {
                            span { class: "conn-confirm", "Delete?" }
                            button {
                                class: "bar-btn danger",
                                onclick: move |_| {
                                    let next = crate::settings::remove_by_id(&saved.read(), cid);
                                    saved.set(next.clone());
                                    save_del(&next);
                                    confirm_delete.set(None);
                                },
                                "Yes"
                            }
                            button {
                                class: "bar-btn",
                                onclick: move |_| confirm_delete.set(None),
                                "Cancel"
                            }
                        } else {
                            button {
                                class: "bar-btn",
                                onclick: move |_| form.set(Some(SshForm::from_conn(&edit))),
                                "Edit"
                            }
                            button {
                                class: "bar-btn danger",
                                onclick: move |_| confirm_delete.set(Some(cid)),
                                "Delete"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The add/edit connection form. Name is required; auth is a Password | Key
/// segmented toggle with secure inputs.
fn connection_form(
    f: SshForm,
    mut form: Signal<Option<SshForm>>,
    mut saved: Signal<Vec<SshConnection>>,
    save_config: impl Fn(&[SshConnection]) + Clone + 'static,
) -> Element {
    let reason = f.invalid_reason();
    let editing = f.id.is_some();
    let title = if editing {
        "Edit connection"
    } else {
        "New connection"
    };
    // Each input writes back through the form signal.
    let mut upd = move |patch: fn(&mut SshForm, String), value: String| {
        if let Some(f) = form.write().as_mut() {
            patch(f, value);
        }
    };
    let is_password = f.auth_kind == AuthKind::Password;

    rsx! {
        div { class: "page-head",
            div { class: "page-title accent", "{title}" }
        }
        div { class: "form",
            label { class: "form-label", "Name *" }
            input {
                class: "form-input",
                placeholder: "prod-gpu",
                value: "{f.name}",
                oninput: move |e| upd(|f, v| f.name = v, e.value()),
            }
            div { class: "form-grid",
                div { class: "form-col wide",
                    label { class: "form-label", "Host *" }
                    input {
                        class: "form-input",
                        placeholder: "gpu-01.lab or 10.0.0.7",
                        value: "{f.host}",
                        oninput: move |e| upd(|f, v| f.host = v, e.value()),
                    }
                }
                div { class: "form-col",
                    label { class: "form-label", "Port" }
                    input {
                        class: "form-input",
                        r#type: "number",
                        placeholder: "22",
                        value: "{f.port}",
                        oninput: move |e| upd(|f, v| f.port = v, e.value()),
                    }
                }
            }
            label { class: "form-label", "User *" }
            input {
                class: "form-input",
                placeholder: "deploy",
                value: "{f.user}",
                oninput: move |e| upd(|f, v| f.user = v, e.value()),
            }

            div { class: "form-section", "AUTHENTICATION" }
            div { class: "auth-toggle",
                button {
                    class: if is_password { "auth-opt active" } else { "auth-opt" },
                    onclick: move |_| {
                        if let Some(f) = form.write().as_mut() {
                            f.auth_kind = AuthKind::Password;
                        }
                    },
                    "Password"
                }
                button {
                    class: if is_password { "auth-opt" } else { "auth-opt active" },
                    onclick: move |_| {
                        if let Some(f) = form.write().as_mut() {
                            f.auth_kind = AuthKind::Key;
                        }
                    },
                    "Key"
                }
            }
            if is_password {
                label { class: "form-label", "Password *" }
                input {
                    class: "form-input",
                    r#type: "password",
                    value: "{f.password}",
                    oninput: move |e| upd(|f, v| f.password = v, e.value()),
                }
            } else {
                label { class: "form-label", "Private key file" }
                input {
                    class: "form-input",
                    placeholder: "~/.ssh/id_ed25519 (empty = default keys)",
                    value: "{f.key_path}",
                    oninput: move |e| upd(|f, v| f.key_path = v, e.value()),
                }
                label { class: "form-label", "Passphrase" }
                input {
                    class: "form-input",
                    r#type: "password",
                    placeholder: "only if the key is encrypted",
                    value: "{f.passphrase}",
                    oninput: move |e| upd(|f, v| f.passphrase = v, e.value()),
                }
            }

            if let Some(r) = reason {
                div { class: "form-hint", "{r}" }
            }
            div { class: "form-actions",
                button {
                    class: "bar-btn",
                    onclick: move |_| form.set(None),
                    "Cancel"
                }
                button {
                    class: if reason.is_none() { "bar-btn accent" } else { "bar-btn disabled" },
                    disabled: reason.is_some(),
                    onclick: {
                        let save = save_config.clone();
                        move |_| {
                            let built = form.read().as_ref().and_then(|f| f.into_connection());
                            if let Some(conn) = built {
                                // Identity-based upsert: edit replaces the entry
                                // sharing its id, add appends with a fresh id —
                                // never keyed on a render-time index.
                                let next = crate::settings::upsert(&saved.read(), conn);
                                saved.set(next.clone());
                                save(&next);
                                form.set(None);
                            }
                        }
                    },
                    "Save"
                }
            }
        }
    }
}

// ── supervisor ────────────────────────────────────────────────────────

/// The supervisor loop: owns the JS bridge, the host actors, and folds every
/// event into the app signals. Runs inside the app coroutine for the lifetime
/// of the window.
async fn supervisor(
    mut app_rx: mpsc::UnboundedReceiver<AppCmd>,
    mut model: Signal<Model>,
    mut status: Signal<Status>,
    mut grid: Signal<(u16, u16)>,
) {
    let desktop = dioxus::desktop::window();
    let mut eval = document::eval(BRIDGE_JS);
    // The first eval can race the page load and vanish without a trace
    // (0.8-alpha); until the bridge says anything, re-issue it periodically.
    // The JS side guards against double-starts and adopts the newest channel.
    let mut bridge_seen = false;
    let (retry_tx, mut retry_rx) = mpsc::unbounded_channel::<()>();
    bg().spawn(async move {
        for _ in 0..10 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if retry_tx.send(()).is_err() {
                break;
            }
        }
    });

    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
    let mut hosts: HashMap<HostId, HostHandle> = HashMap::new();
    // Remembered so a Down host can be respawned on Reconnect.
    let mut kinds: HashMap<HostId, HostKind> = HashMap::new();
    let mut active: Option<HostId> = None;
    let mut bridge_ready = false;
    // A session picked before the JS bridge was ready (auto-select on the
    // first local list): attached once the bridge reports in.
    let mut pending_select: Option<(HostId, String)> = None;
    // Set when the user asks for a new session, cleared when they select any
    // session first: gates whether an arriving `Created` steals focus.
    let mut pending_create = false;
    // The session named on the command line, consumed by the first auto-select.
    let mut preferred = crate::preferred_session();

    spawn_host(LOCAL_ID, HostKind::Local, &ui_tx, &mut hosts, &mut kinds);

    loop {
        tokio::select! {
            Some(()) = retry_rx.recv() => {
                if !bridge_seen {
                    tracing::warn!("bridge silent — re-issuing eval");
                    eval = document::eval(BRIDGE_JS);
                }
            }
            msg = eval.recv() => {
                let Ok(val) = msg else {
                    tracing::error!("eval bridge closed");
                    break;
                };
                bridge_seen = true;
                match serde_json::from_value::<JsMessage>(val) {
                    Ok(JsMessage::Status { msg }) => {
                        tracing::info!("bridge: {msg}");
                        // The JS side rebuilt a wedged terminal: re-attach the
                        // active session so a fresh Snapshot repopulates it.
                        if msg.contains("terminal recreated")
                            && let Some((host, name)) = model.read().active.clone()
                        {
                            let (cols, rows) = *grid.read();
                            route(
                                AppCmd::SetActive { host, name, cols, rows },
                                &ui_tx, &mut hosts, &mut kinds, &mut active,
                            );
                        }
                        if msg.contains("bridge ready") {
                            bridge_ready = true;
                            if let Some((host, name)) = pending_select.take() {
                                let (cols, rows) = *grid.read();
                                route(
                                    AppCmd::SetActive { host, name, cols, rows },
                                    &ui_tx, &mut hosts, &mut kinds, &mut active,
                                );
                            }
                        }
                    }
                    Ok(JsMessage::Input { data }) => {
                        route(
                            AppCmd::Input(data.into_bytes()),
                            &ui_tx, &mut hosts, &mut kinds, &mut active,
                        );
                    }
                    Ok(JsMessage::Resize { cols, rows }) => {
                        grid.set((cols, rows));
                        route(
                            AppCmd::Resize { cols, rows },
                            &ui_tx, &mut hosts, &mut kinds, &mut active,
                        );
                    }
                    Err(e) => tracing::warn!("bridge unexpected msg: {e}"),
                }
            }
            cmd = app_rx.recv() => {
                let Some(cmd) = cmd else { break };
                // Track whether the user is waiting for a fresh session: a
                // manual select cancels the pending "+ new" focus jump.
                match &cmd {
                    AppCmd::Create { .. } => pending_create = true,
                    AppCmd::SetActive { .. } => pending_create = false,
                    _ => {}
                }
                route(cmd, &ui_tx, &mut hosts, &mut kinds, &mut active);
            }
            ev = ui_rx.recv() => {
                let Some(ev) = ev else { break };
                match ev {
                    UiEvent::State { host, state } => {
                        // If the host of the session we're viewing dropped,
                        // reflect it in the terminal header.
                        if let HostState::Down(msg) = &state
                            && model.read().active.as_ref().is_some_and(|(h, _)| *h == host)
                        {
                            status.set(Status::Disconnected(msg.clone()));
                        }
                        model.write().set_state(host, state);
                    }
                    UiEvent::Sessions { host, sessions } => {
                        let had_active = model.read().active.is_some();
                        model.write().set_sessions(host, sessions);
                        // The viewed session vanished (killed/exited elsewhere)
                        // → its selection was cleared; blank the pane so its
                        // last frame doesn't linger.
                        if had_active && model.read().active.is_none() {
                            let _ = desktop.webview.evaluate_script("window.__asdReset&&window.__asdReset();");
                        }
                        // On the local host's first populate, auto-select
                        // something so the window isn't empty.
                        if model.read().active.is_none()
                            && pending_select.is_none()
                            && host == LOCAL_ID
                        {
                            // Prefer the session named on the command line
                            // when it exists, otherwise take the first one.
                            let pick = model.read().host(LOCAL_ID).and_then(|h| {
                                preferred
                                    .take_if(|p| h.sessions.iter().any(|s| &s.name == p))
                                    .or_else(|| h.sessions.first().map(|s| s.name.clone()))
                            });
                            if let Some(name) = pick {
                                model.write().select(LOCAL_ID, name.clone());
                                status.set(Status::Live);
                                if bridge_ready {
                                    let (cols, rows) = *grid.read();
                                    route(
                                        AppCmd::SetActive { host: LOCAL_ID, name, cols, rows },
                                        &ui_tx, &mut hosts, &mut kinds, &mut active,
                                    );
                                } else {
                                    pending_select = Some((LOCAL_ID, name));
                                }
                            }
                        }
                    }
                    UiEvent::Created { host, name } => {
                        // Only jump to the new session if the user is still
                        // waiting for it — if they clicked another session after
                        // "+ new", don't yank the view away. The list poll shows
                        // the new session either way. No reset here: the attach
                        // Snapshot resets before it repopulates the pane.
                        if pending_create {
                            pending_create = false;
                            model.write().select(host, name.clone());
                            status.set(Status::Live);
                            let (cols, rows) = *grid.read();
                            route(
                                AppCmd::SetActive { host, name, cols, rows },
                                &ui_tx, &mut hosts, &mut kinds, &mut active,
                            );
                        }
                    }
                    UiEvent::Bytes { host, name, data, snapshot } => {
                        // Gate on the session too: bytes from the one we just
                        // left can still be in flight right after a switch.
                        if !model.read().is_active(host, &name) {
                            continue;
                        }
                        // A JSON string is a valid JS string literal
                        // (control chars come out as backslash-u escapes), so
                        // it is passed directly - routing it through
                        // JSON.parse would un-escape twice and choke on ESC.
                        let json = serde_json::to_string(&String::from_utf8_lossy(&data))
                            .unwrap_or_else(|_| "\"\"".into());
                        let script = if snapshot {
                            format!(
                                "window.__asdReset&&window.__asdReset();window.__asdWrite&&window.__asdWrite({json});"
                            )
                        } else {
                            format!("window.__asdWrite&&window.__asdWrite({json});")
                        };
                        if let Err(e) = desktop.webview.evaluate_script(&script) {
                            tracing::error!("evaluate_script: {e}");
                        }
                    }
                    UiEvent::SessionEnded { host, name, msg } => {
                        if model.read().is_active(host, &name) {
                            status.set(Status::Ended(msg));
                            // Blank the pane — the dead session's last frame
                            // must not linger under the "ended" note.
                            let _ = desktop.webview.evaluate_script("window.__asdReset&&window.__asdReset();");
                        }
                    }
                }
            }
            else => break,
        }
    }
}

/// Route one app command to the right host actor.
fn route(
    cmd: AppCmd,
    ui_tx: &UnboundedSender<UiEvent>,
    hosts: &mut HashMap<HostId, HostHandle>,
    kinds: &mut HashMap<HostId, HostKind>,
    active: &mut Option<HostId>,
) {
    match cmd {
        AppCmd::AddRemote { id, spec } => {
            if !hosts.contains_key(&id) {
                spawn_host(id, HostKind::Ssh(spec), ui_tx, hosts, kinds);
            }
        }
        AppCmd::RemoveHost { id } => {
            if let Some(h) = hosts.remove(&id) {
                let _ = h.cmd_tx.send(HostCmd::Shutdown);
            }
            kinds.remove(&id);
            if *active == Some(id) {
                *active = None;
            }
        }
        AppCmd::Reconnect { id } => {
            if let Some(kind) = kinds.get(&id).cloned() {
                if let Some(h) = hosts.remove(&id) {
                    let _ = h.cmd_tx.send(HostCmd::Shutdown);
                }
                spawn_host(id, kind, ui_tx, hosts, kinds);
            }
        }
        AppCmd::SetActive {
            host,
            name,
            cols,
            rows,
        } => {
            if let Some(prev) = *active
                && prev != host
                && let Some(h) = hosts.get(&prev)
            {
                let _ = h.cmd_tx.send(HostCmd::Detach);
            }
            *active = Some(host);
            if let Some(h) = hosts.get(&host) {
                let _ = h.cmd_tx.send(HostCmd::Attach { name, cols, rows });
            }
        }
        AppCmd::Input(bytes) => {
            if let Some(a) = *active
                && let Some(h) = hosts.get(&a)
            {
                let _ = h.cmd_tx.send(HostCmd::Input(bytes));
            }
        }
        AppCmd::Resize { cols, rows } => {
            if let Some(a) = *active
                && let Some(h) = hosts.get(&a)
            {
                let _ = h.cmd_tx.send(HostCmd::Resize { cols, rows });
            }
        }
        AppCmd::Create { host } => {
            if let Some(h) = hosts.get(&host) {
                let _ = h.cmd_tx.send(HostCmd::Create);
            }
        }
        AppCmd::Kill { host, name } => {
            if let Some(h) = hosts.get(&host) {
                let _ = h.cmd_tx.send(HostCmd::Kill { name });
            }
        }
        AppCmd::Rename {
            host,
            name,
            new_name,
        } => {
            if let Some(h) = hosts.get(&host) {
                let _ = h.cmd_tx.send(HostCmd::Rename { name, new_name });
            }
        }
    }
}

fn spawn_host(
    id: HostId,
    kind: HostKind,
    ui_tx: &UnboundedSender<UiEvent>,
    hosts: &mut HashMap<HostId, HostHandle>,
    kinds: &mut HashMap<HostId, HostKind>,
) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<HostCmd>();
    let ev = ui_tx.clone();
    kinds.insert(id, kind.clone());
    // On the bg runtime: the actor's list-poll ticker (and russh) need a
    // runtime that keeps running while the window is idle.
    bg().spawn(conn::run_host(id, kind, cmd_rx, ev));
    hosts.insert(id, HostHandle { cmd_tx });
}
