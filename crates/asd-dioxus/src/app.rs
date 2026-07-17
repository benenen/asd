//! Main application component: host-grouped session sidebar + ghostty-web
//! terminal pane + status bar, plus the settings overlay (saved SSH
//! connections). Mirrors `asd-gui`'s M2 layout and semantics.
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
    HostId, HostKind, HostState, LOCAL_ID, Model, RemoteSpec, short_age, short_cmd, short_reason,
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
}

/// State of the active session's stream, shown in the terminal header.
#[derive(Debug, Clone, PartialEq)]
enum Status {
    Live,
    Ended(String),
    Disconnected(String),
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
                user: conn.user.clone(),
                host: conn.host.clone(),
                port: conn.port,
                auth: conn.auth.clone(),
                name: conn.name.clone(),
            };
            if model.read().has_remote(&spec.user, &spec.host, spec.port) {
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
                        for (i, conn) in saved_list.iter().enumerate() {
                            {
                                let added = model.read().has_remote(&conn.user, &conn.host, conn.port);
                                let conn2 = conn.clone();
                                let mut add = add_saved.clone();
                                rsx! {
                                    div {
                                        key: "conn-{i}",
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
                        {host_group(host, &active, now, select_session.clone(), tx.clone(), model, status)}
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
                            onclick: {
                                let tx = tx.clone();
                                move |_| {
                                    if let Some((host, name)) = model.read().active.clone() {
                                        let _ = tx.send(AppCmd::Kill { host, name });
                                    }
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
                {settings_view(settings_page, form, saved_ssh, model, tx.clone(), settings_open, save_config)}
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
fn host_group(
    host: &crate::model::Host,
    active: &Option<(HostId, String)>,
    now: u64,
    select: impl FnMut(HostId, String) + Clone + 'static,
    tx: UnboundedSender<AppCmd>,
    mut model: Signal<Model>,
    _status: Signal<Status>,
) -> Element {
    let id = host.id;
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
    let down_reason = match &state {
        HostState::Down(msg) => Some(short_reason(msg)),
        _ => None,
    };
    let is_down = down_reason.is_some();

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
            if let Some(reason) = down_reason {
                div { class: "host-reason", "{reason}" }
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
                    let tx_kill = tx.clone();
                    let row = if is_active { "session-row active" } else { "session-row" };
                    let sdot = if attached { "session-dot attached" } else { "session-dot" };
                    let sdot = if remote { format!("{sdot} remote") } else { sdot.to_string() };
                    rsx! {
                        div {
                            class: "{row}",
                            key: "s-{id}-{name}",
                            onclick: move |_| select(id, name_click.clone()),
                            span { class: "{sdot}" }
                            span { class: "session-name", "{name}" }
                            span { class: "session-cmd", "{cmd}" }
                            span { class: "session-age", "{age}" }
                            button {
                                class: "icon-btn kill",
                                title: "kill {name}",
                                onclick: move |e| {
                                    e.stop_propagation();
                                    let _ = tx_kill.send(AppCmd::Kill { host: id, name: name_kill.clone() });
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
                            connections_page(form, saved, model, save_config.clone())
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
        for (i, c) in list.iter().enumerate() {
            {
                let added = model.read().has_remote(&c.user, &c.host, c.port);
                let edit = c.clone();
                let save_del = save_config.clone();
                rsx! {
                    div { class: "conn-row", key: "saved-{i}",
                        div { class: "conn-main",
                            div { class: "conn-name", "{c.name}" }
                            div { class: "conn-sub", "{c.label()}" }
                        }
                        span { class: "conn-auth", "{c.auth.tag()}" }
                        if added {
                            span { class: "connect-tag", "added" }
                        }
                        button {
                            class: "bar-btn",
                            onclick: move |_| form.set(Some(SshForm::from_conn(&edit, i))),
                            "Edit"
                        }
                        button {
                            class: "bar-btn danger",
                            onclick: move |_| {
                                let mut list = saved.write();
                                if i < list.len() {
                                    list.remove(i);
                                }
                                save_del(&list);
                            },
                            "Delete"
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
    let editing = f.index.is_some();
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
                            let built = form.read().as_ref().and_then(|f| {
                                f.into_connection().map(|c| (f.index, c))
                            });
                            if let Some((index, conn)) = built {
                                let mut list = saved.write();
                                match index {
                                    Some(i) if i < list.len() => list[i] = conn,
                                    _ => list.push(conn),
                                }
                                save(&list);
                                drop(list);
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
                        model.write().set_sessions(host, sessions);
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
                        // The user asked for it — show it.
                        model.write().select(host, name.clone());
                        status.set(Status::Live);
                        let (cols, rows) = *grid.read();
                        let _ = desktop.webview.evaluate_script("window.__asdReset&&window.__asdReset();");
                        route(
                            AppCmd::SetActive { host, name, cols, rows },
                            &ui_tx, &mut hosts, &mut kinds, &mut active,
                        );
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
                        }
                    }
                }
            }
            else => break,
        }
    }
}

/// Route one app command to the right host actor (mirrors `asd-gui`'s route).
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
