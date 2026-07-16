//! `asd-gui`: the GPU terminal client (spec §7), M2 two-pane redesign.
//!
//! A host-grouped session sidebar (local + SSH remotes) beside the live
//! terminal. One background **supervisor** (in the iced subscription) owns a
//! per-host actor thread (see [`conn`]); each actor keeps its own `!Send`
//! terminal off iced's multi-threaded runtime. The app holds only plain data
//! ([`model::Model`] + the active [`RenderSnapshot`]) and routes [`AppCmd`]s to
//! the supervisor.
//!
//! Shipped as a library so the single `asd` binary can embed it behind the
//! `gui` feature ([`run`] is the entry point); this crate stays free of
//! portable-pty/process-management so a GUI-only build (Windows) is viable.

mod conn;
mod ime;
mod key;
mod model;
mod render;
pub(crate) mod settings;
mod ssh;
mod theme;
mod view;

use std::collections::HashMap;
use std::time::Duration;

use asd_vt::{KeyEvent, RenderSnapshot};
use conn::{HostCmd, HostHandle, UiEvent};
use iced::futures::stream::BoxStream;
use iced::futures::{SinkExt, StreamExt};
use iced::widget::canvas;
use iced::{Element, Point, Size, Subscription, Task};
use model::{HostId, HostKind, HostState, LOCAL_ID, Model, RemoteSpec};
use settings::{SettingsConfig, SettingsMsg, SettingsPage, SshConnection, SshForm};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Launch the GUI. `session`, when given (`asd gui <session>`), is pre-selected
/// on the local host once its list arrives. This is the `gui`-feature entry
/// point of the single `asd` binary.
pub fn run(session: Option<String>) -> iced::Result {
    // try_init: the embedding binary may already have a subscriber installed.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .try_init();

    iced::application(move || App::new(session.clone()), App::update, App::view)
        .subscription(App::subscription)
        .title(App::title)
        .antialiasing(true)
        // Bundled symbol fallback: the monospace face lacks media/technical
        // glyphs (e.g. U+23F5 ⏵, arrows, misc symbols), and many systems — the
        // Windows client especially — have no font that covers them, so those
        // cells would render as tofu boxes. Loading Noto Sans Symbols 2 into the
        // font DB gives cosmic-text something to fall back to on every platform.
        .font(include_bytes!("../fonts/NotoSansSymbols2-Regular.ttf").as_slice())
        // Keep the startup size in sync with App::new's initial grid estimate.
        .window_size(iced::Size::new(960.0, 600.0))
        .run()
}

/// Commands the app sends to the supervisor.
#[derive(Debug, Clone)]
pub(crate) enum AppCmd {
    AddRemote {
        id: HostId,
        spec: RemoteSpec,
    },
    RemoveHost {
        id: HostId,
    },
    SetActive {
        host: HostId,
        name: String,
        cols: u16,
        rows: u16,
    },
    Key(KeyEvent),
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
    /// Set the terminal's scrollback offset (0 = follow live output).
    Scroll(usize),
}

/// State of the *active* session's stream (the terminal pane). When nothing is
/// selected the pane shows a hint regardless of this.
pub(crate) enum Status {
    Live,
    Ended(String),
    Disconnected(String),
}

/// Extract plain text from cells within an absolute-anchored selection,
/// projecting to the current viewport (`base` = viewport's absolute top row).
fn extract_selection_text(snap: &RenderSnapshot, sel: GuiSelection, base: usize) -> String {
    let (a, b) = if (sel.anchor.1, sel.anchor.0) <= (sel.head.1, sel.head.0) {
        (sel.anchor, sel.head)
    } else {
        (sel.head, sel.anchor)
    };
    // Project absolute rows to viewport rows (vpy = abs_row − base), clamping
    // ends scrolled out of frame — only the visible cells are available.
    let rows = snap.rows;
    let vpy_a = match crate::render::viewport_row(a.1, base, rows) {
        Some(v) => v,
        None if a.1 < base => 0,      // top above view → clamp to first
        None => return String::new(), // whole selection below view
    };
    let vpy_b = match crate::render::viewport_row(b.1, base, rows) {
        Some(v) => v,
        None if b.1 >= base => rows.saturating_sub(1) as usize, // bottom below → last row
        None => return String::new(),                           // whole selection above view
    };
    if vpy_a > vpy_b {
        return String::new();
    }
    let mut out: Vec<String> = Vec::new();
    for y in vpy_a..=vpy_b {
        let row_cells = match snap.cells.get(y) {
            Some(r) => r,
            None => continue,
        };
        let start_x: usize = if y == vpy_a { a.0 as usize } else { 0 };
        let end_x: usize = if y == vpy_b {
            b.0 as usize
        } else {
            snap.cols.saturating_sub(1) as usize
        };
        let mut line = String::new();
        let end_x = end_x.min(row_cells.len().saturating_sub(1));
        // `.get()` yields None (not a panic) when start_x > end_x, e.g. an empty
        // row; that row contributes an empty line, matching the old behavior.
        for cell in row_cells.get(start_x..=end_x).into_iter().flatten() {
            if cell.width == asd_vt::CellWidth::SpacerTail {
                continue;
            }
            if cell.grapheme.is_empty() {
                line.push(' ');
            } else {
                line.push_str(&cell.grapheme);
            }
        }
        while line.ends_with(' ') {
            line.pop();
        }
        out.push(line);
    }
    out.join("\n")
}

pub(crate) struct App {
    pub(crate) model: Model,
    pub(crate) frame: Option<RenderSnapshot>,
    pub(crate) cache: canvas::Cache,
    pub(crate) metrics: render::Metrics,
    pub(crate) status: Status,
    pub(crate) live_cols: u16,
    pub(crate) live_rows: u16,
    pub(crate) window: Size,
    /// Whether the "add a saved SSH connection" menu (opened by the `+` next to
    /// the HOSTS header) is showing.
    pub(crate) connect_menu_open: bool,
    pub(crate) now_ms: u64,
    /// Scrollback offset: 0 = following live output.
    pub(crate) scroll: usize,
    /// Absolute row of the current frame's viewport top (`scrollback_rows −
    /// scroll`), reported by the host actor with each frame. Selections are
    /// anchored in this absolute space and projected back through it so the
    /// highlight tracks the text while scrolling.
    pub(crate) frame_base: usize,
    /// Current drag selection anchored in absolute scrollback rows, if any.
    pub(crate) selection: Option<GuiSelection>,
    /// Whether the active session is in mouse-tracking mode (vim/htop).
    pub(crate) session_wants_mouse: bool,
    /// Last known mouse position in the terminal pane (pixels).
    pub(crate) last_mouse_pos: Option<(f32, f32)>,
    sup_tx: Option<UnboundedSender<AppCmd>>,
    generation: u64,
    /// A session named on the command line (`asd gui <session>`) to auto-select
    /// once the local host's list arrives; cleared after it is honored.
    preferred: Option<String>,
    // ── settings ──
    pub(crate) show_settings: bool,
    pub(crate) settings_page: SettingsPage,
    pub(crate) settings_form: Option<SshForm>,
    pub(crate) saved_ssh: Vec<SshConnection>,
}

/// A drag selection anchored in **absolute scrollback rows**: each end is
/// `(col, abs_row)` where `abs_row = frame_base + viewport_row` at the moment
/// it was set (`frame_base` = scrollback_rows − scroll, 0 = oldest line).
/// Anchoring absolute — rather than to the viewport row — makes the highlight
/// follow the text as the viewport scrolls, matching the CLI's content-pin
/// selection. Projected back to a viewport row via [`render::viewport_row`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct GuiSelection {
    pub anchor: (u16, usize),
    pub head: (u16, usize),
}

#[derive(Debug, Clone)]
pub(crate) enum Message {
    /// The supervisor started; carries the channel to command it.
    Supervisor(UnboundedSender<AppCmd>),
    Ui(UiEvent),
    Select(HostId, String),
    NewSession(HostId),
    Kill(HostId, String),
    RemoveHost(HostId),
    /// Toggle the "add saved SSH connection" menu next to the HOSTS header.
    ToggleConnectMenu,
    /// Connect the saved SSH connection at this index in `saved_ssh`.
    ConnectSaved(usize),
    /// Open the settings panel on the Connections page (to configure a host).
    OpenConnections,
    /// Restart every host connection (the local daemon can't be auto-spawned
    /// from here, so this is how the user recovers after starting it).
    Reconnect,
    Keyboard(iced::keyboard::Event),
    Resized(Size),
    Tick,
    /// Mouse wheel scroll (positive = up).
    MouseScroll(i32),
    /// Mouse press at (x, y) in cell coordinates.
    MousePress,
    /// Mouse drag to (x, y).
    MouseMove(Point),
    /// Mouse release: finalizes and copies the selection.
    MouseRelease,
    /// IME-composed text committed (e.g. a CJK character confirmed by the user).
    ImeCommit(String),
    // ── settings ──
    ToggleSettings,
    Settings(SettingsMsg),
}

impl App {
    fn new(preferred: Option<String>) -> (Self, Task<Message>) {
        let metrics = render::Metrics::new(14.0);
        // iced doesn't emit a resize on startup, so seed the grid from the
        // default window minus the chrome — otherwise the first attach sizes to
        // a stale 80×24 until the user resizes.
        let window = Size::new(960.0, 600.0);
        let w = (window.width - view::SIDEBAR_W).max(1.0);
        let h = (window.height - view::STATUS_H - view::TERMHEAD_H).max(1.0);
        let (live_cols, live_rows) = metrics.grid(Size::new(w, h));
        let config = SettingsConfig::load();
        (
            Self {
                model: Model::with_local(),
                frame: None,
                cache: canvas::Cache::new(),
                metrics,
                status: Status::Live,
                live_cols,
                live_rows,
                window,
                connect_menu_open: false,
                now_ms: now_ms(),
                sup_tx: None,
                generation: 0,
                preferred,
                scroll: 0,
                frame_base: 0,
                selection: None,
                session_wants_mouse: false,
                last_mouse_pos: None,
                show_settings: false,
                settings_page: SettingsPage::General,
                settings_form: None,
                saved_ssh: config.ssh_connections,
            },
            Task::none(),
        )
    }

    fn title(&self) -> String {
        match &self.model.active {
            Some((_, name)) => format!("asd — {name}"),
            None => "asd".to_string(),
        }
    }

    /// Terminal columns/rows that fit the terminal pane (window minus chrome).
    fn grid(&self) -> (u16, u16) {
        let w = (self.window.width - view::SIDEBAR_W).max(1.0);
        let h = (self.window.height - view::STATUS_H - view::TERMHEAD_H).max(1.0);
        self.metrics.grid(Size::new(w, h))
    }

    fn send(&self, cmd: AppCmd) {
        if let Some(tx) = &self.sup_tx {
            let _ = tx.send(cmd);
        }
    }

    fn attach(&mut self, host: HostId, name: String) {
        let (cols, rows) = (self.live_cols, self.live_rows);
        self.model.select(host, name.clone());
        self.status = Status::Live;
        self.scroll = 0;
        self.selection = None;
        self.frame = None;
        self.cache.clear();
        self.send(AppCmd::SetActive {
            host,
            name,
            cols,
            rows,
        });
    }

    /// Persist the current SSH connection list to disk.
    fn save_ssh_config(&self) {
        let config = SettingsConfig {
            ssh_connections: self.saved_ssh.clone(),
        };
        config.save();
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Supervisor(tx) => {
                self.sup_tx = Some(tx);
                // Re-establish any remote hosts and the active session after a
                // (re)start of the supervisor.
                let remotes: Vec<(HostId, RemoteSpec)> = self
                    .model
                    .hosts
                    .iter()
                    .filter_map(|h| match &h.kind {
                        HostKind::Ssh(s) => Some((h.id, s.clone())),
                        HostKind::Local => None,
                    })
                    .collect();
                for (id, spec) in remotes {
                    self.send(AppCmd::AddRemote { id, spec });
                }
                if let Some((host, name)) = self.model.active.clone() {
                    let (cols, rows) = (self.live_cols, self.live_rows);
                    self.send(AppCmd::SetActive {
                        host,
                        name,
                        cols,
                        rows,
                    });
                }
            }
            Message::Ui(UiEvent::State { host, state }) => {
                self.model.set_state(host, state.clone());
                // If the host of the session we're viewing dropped, reflect it.
                if let HostState::Down(msg) = &state
                    && self.model.active.as_ref().is_some_and(|(h, _)| *h == host)
                {
                    self.status = Status::Disconnected(msg.clone());
                }
            }
            Message::Ui(UiEvent::Sessions { host, sessions }) => {
                self.model.set_sessions(host, sessions);
                // On the local host's first populate, auto-select something so
                // the window isn't empty: the session named on the command line
                // if it exists, otherwise the first one.
                if self.model.active.is_none() && host == LOCAL_ID {
                    let names: Vec<String> = self
                        .model
                        .host(LOCAL_ID)
                        .map(|h| h.sessions.iter().map(|s| s.name.clone()).collect())
                        .unwrap_or_default();
                    if let Some(first) = names.first() {
                        let pick = self
                            .preferred
                            .take()
                            .filter(|name| names.contains(name))
                            .unwrap_or_else(|| first.clone());
                        self.attach(LOCAL_ID, pick);
                    }
                }
            }
            Message::Ui(UiEvent::Created { host, name }) => {
                // The user asked for it — show it.
                self.attach(host, name);
            }
            Message::Ui(UiEvent::Frame {
                host,
                name,
                snap,
                session_wants_mouse,
                base,
            }) => {
                // Gate on the session too: frames from the one we just left
                // can still be in flight right after a switch.
                if self.model.is_active(host, &name) {
                    self.session_wants_mouse = session_wants_mouse;
                    self.frame_base = base;
                    self.frame = Some(*snap);
                    self.cache.clear();
                    self.status = Status::Live;
                }
            }
            Message::Ui(UiEvent::SessionEnded { host, name, msg }) => {
                if self.model.is_active(host, &name) {
                    self.status = Status::Ended(msg);
                }
            }
            Message::Select(host, name) => {
                if !self.model.is_active(host, &name) {
                    self.attach(host, name);
                }
            }
            Message::NewSession(host) => self.send(AppCmd::Create { host }),
            Message::Kill(host, name) => self.send(AppCmd::Kill { host, name }),
            Message::RemoveHost(id) => {
                self.model.remove_host(id);
                self.send(AppCmd::RemoveHost { id });
            }
            Message::Reconnect => {
                // Bumping the generation changes the subscription seed's hash,
                // so iced restarts the supervisor; App::update replays the
                // remote hosts and active session on the fresh Supervisor.
                self.generation += 1;
                for h in &mut self.model.hosts {
                    h.state = HostState::Connecting;
                }
            }
            Message::ToggleConnectMenu => self.connect_menu_open = !self.connect_menu_open,
            Message::ConnectSaved(i) => {
                self.connect_menu_open = false;
                if let Some(conn) = self.saved_ssh.get(i) {
                    // Ignore a click on a host that is already in the list (the
                    // menu also disables it, but guard here too).
                    if self.model.has_remote(&conn.user, &conn.host, conn.port) {
                        return Task::none();
                    }
                    let spec = RemoteSpec {
                        user: conn.user.clone(),
                        host: conn.host.clone(),
                        port: conn.port,
                        auth: conn.auth.clone(),
                        name: conn.name.clone(),
                    };
                    let id = self.model.add_remote(spec.clone());
                    self.send(AppCmd::AddRemote { id, spec });
                }
            }
            Message::OpenConnections => {
                self.connect_menu_open = false;
                self.show_settings = true;
                self.settings_page = SettingsPage::Connections;
            }
            Message::Keyboard(iced::keyboard::Event::KeyPressed {
                modified_key,
                modifiers,
                text,
                ..
            }) => {
                if self.model.active.is_some() {
                    // Typing snaps back to the live bottom and clears any selection.
                    if self.scroll != 0 {
                        self.scroll = 0;
                        self.send(AppCmd::Scroll(0));
                    }
                    self.selection = None;
                    // Use `modified_key` (Shift/CapsLock/AltGr applied, Ctrl
                    // excluded) rather than the bare `key`: the base key is
                    // case- and layout-unshifted, so `Shift+a` would send `a`
                    // and `Shift+1` would send `1` instead of `!`. Ctrl is kept
                    // in `modifiers` so Ctrl+C still encodes as ^C.
                    if let Some(ev) = key::map_key(&modified_key, modifiers) {
                        self.send(AppCmd::Key(ev));
                    } else if let Some(t) = text {
                        // IME-committed text (e.g. CJK characters) arrives as
                        // KeyPressed with key=Unidentified and text set.
                        // Send each character individually so the PTY gets
                        // one codepoint per write.
                        for ch in t.chars() {
                            if let Some(ev) = key::map_ime_char(ch, modifiers) {
                                self.send(AppCmd::Key(ev));
                            }
                        }
                    }
                }
            }
            Message::Keyboard(_) => {}
            Message::Resized(size) => {
                self.window = size;
                let (cols, rows) = self.grid();
                if (cols, rows) != (self.live_cols, self.live_rows) {
                    self.live_cols = cols;
                    self.live_rows = rows;
                    self.send(AppCmd::Resize { cols, rows });
                }
            }
            Message::Tick => self.now_ms = now_ms(),
            Message::MouseScroll(delta) => {
                if self.model.active.is_some() {
                    // TODO: when session_wants_mouse && scroll == 0, forward
                    // wheel events to the session as SGR mouse reports instead
                    // of scrolling locally.
                    let max = 100_000;
                    let new_scroll = if delta > 0 {
                        (self.scroll + delta as usize).min(max)
                    } else {
                        self.scroll.saturating_sub((-delta) as usize)
                    };
                    if new_scroll != self.scroll {
                        self.scroll = new_scroll;
                        // Don't invalidate the canvas cache here — the frame
                        // data hasn't changed yet (the daemon hasn't sent the
                        // updated viewport). Clearing it would trigger a redraw
                        // with stale cells, then ANOTHER redraw when the frame
                        // arrives, causing a double-paint flicker that feels
                        // jerky when switching scroll directions. Instead, let
                        // the frame update drive cache invalidation.
                        self.send(AppCmd::Scroll(self.scroll));
                    }
                }
            }
            Message::MousePress => {
                // Drag always starts a local selection, even when the session
                // has mouse tracking on (vim/htop/claude/codex). Forwarding the
                // mouse *to* the session isn't implemented yet, so guarding it
                // off just made those (very common) sessions unselectable — a
                // plain click makes a zero-width selection that copies nothing,
                // so this costs nothing until mouse-forwarding lands (which can
                // then gate on a modifier, e.g. Shift to select / bare to send).
                if self.model.active.is_some()
                    && let Some((px, py)) = self.last_mouse_pos
                {
                    // Pixel → cell, then anchor the viewport row in absolute
                    // scrollback space so the selection tracks the text.
                    let x = self.metrics.col_at(px);
                    let abs = render::abs_row(self.frame_base, self.metrics.row_at(py) as usize);
                    self.selection = Some(GuiSelection {
                        anchor: (x, abs),
                        head: (x, abs),
                    });
                }
            }
            Message::MouseMove(pos) => {
                self.last_mouse_pos = Some((pos.x, pos.y));
                // Only update the local drag selection; when session wants
                // mouse we don't start one (see MousePress).
                if self.selection.is_some() {
                    let x = self.metrics.col_at(pos.x);
                    let abs = render::abs_row(self.frame_base, self.metrics.row_at(pos.y) as usize);
                    if let Some(ref mut sel) = self.selection {
                        sel.head = (x, abs);
                    }
                }
            }
            Message::MouseRelease => {
                if let Some(sel) = self.selection.take()
                    && let Some(ref snap) = self.frame
                {
                    let text = extract_selection_text(snap, sel, self.frame_base);
                    if !text.is_empty() {
                        return iced::clipboard::write(text);
                    }
                }
            }
            Message::ImeCommit(text) => {
                // Forward IME-composed text to the session as individual
                // character key events, same as the keyboard handler does
                // for committed IME text via KeyPressed.
                if self.model.active.is_some() {
                    if self.scroll != 0 {
                        self.scroll = 0;
                        self.send(AppCmd::Scroll(0));
                    }
                    self.selection = None;
                    for ch in text.chars() {
                        if ch.is_control() {
                            continue;
                        }
                        let ev = asd_vt::KeyEvent {
                            key: asd_vt::Key::Char(ch),
                            mods: asd_vt::Mods {
                                shift: false,
                                ctrl: false,
                                alt: false,
                                super_key: false,
                            },
                            text: Some(ch.to_string()),
                        };
                        self.send(AppCmd::Key(ev));
                    }
                }
            }
            // ── settings ──
            Message::ToggleSettings => {
                self.show_settings = !self.show_settings;
                if !self.show_settings {
                    self.settings_form = None;
                }
            }
            Message::Settings(msg) => match msg {
                SettingsMsg::Close => {
                    self.show_settings = false;
                    self.settings_form = None;
                }
                SettingsMsg::Nav(page) => {
                    self.settings_page = page;
                    self.settings_form = None;
                }
                SettingsMsg::AddConnection => {
                    self.settings_form = Some(SshForm::default());
                }
                SettingsMsg::EditConnection(i) => {
                    if let Some(conn) = self.saved_ssh.get(i) {
                        self.settings_form = Some(SshForm::from_conn(conn, i));
                    }
                }
                SettingsMsg::DeleteConnection(i) => {
                    if i < self.saved_ssh.len() {
                        self.saved_ssh.remove(i);
                        self.save_ssh_config();
                    }
                }
                SettingsMsg::SaveConnection => {
                    if let Some(ref form) = self.settings_form
                        && let Some(conn) = form.into_connection()
                    {
                        if let Some(i) = form.index
                            && i < self.saved_ssh.len()
                        {
                            self.saved_ssh[i] = conn;
                        } else {
                            self.saved_ssh.push(conn);
                        }
                        self.save_ssh_config();
                        self.settings_form = None;
                    }
                }
                SettingsMsg::CancelEdit => {
                    self.settings_form = None;
                }
                SettingsMsg::FormName(s) => {
                    if let Some(ref mut f) = self.settings_form {
                        f.name = s;
                    }
                }
                SettingsMsg::FormHost(s) => {
                    if let Some(ref mut f) = self.settings_form {
                        f.host = s;
                    }
                }
                SettingsMsg::FormUser(s) => {
                    if let Some(ref mut f) = self.settings_form {
                        f.user = s;
                    }
                }
                SettingsMsg::FormPort(s) => {
                    if let Some(ref mut f) = self.settings_form {
                        f.port = s;
                    }
                }
                SettingsMsg::FormAuthKind(k) => {
                    if let Some(ref mut f) = self.settings_form {
                        f.auth_kind = k;
                    }
                }
                SettingsMsg::FormPassword(s) => {
                    if let Some(ref mut f) = self.settings_form {
                        f.password = s;
                    }
                }
                SettingsMsg::FormKeyPath(s) => {
                    if let Some(ref mut f) = self.settings_form {
                        f.key_path = s;
                    }
                }
                SettingsMsg::FormPassphrase(s) => {
                    if let Some(ref mut f) = self.settings_form {
                        f.passphrase = s;
                    }
                }
            },
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::keyboard::listen().map(Message::Keyboard),
            iced::window::resize_events().map(|(_, size)| Message::Resized(size)),
            iced::time::every(Duration::from_secs(15)).map(|_| Message::Tick),
            Subscription::run_with(
                Seed {
                    generation: self.generation,
                },
                supervisor,
            ),
        ])
    }

    fn view(&self) -> Element<'_, Message> {
        let body = view::view(self, self.selection);
        if self.show_settings {
            // The settings overlay is built in view.rs where all widget
            // helpers are available.
            let overlay =
                view::settings_overlay(self.settings_page, &self.saved_ssh, &self.settings_form);
            iced::widget::stack([body, overlay]).into()
        } else {
            body
        }
    }
}

/// The subscription seed; bumping `generation` restarts the whole supervisor.
#[derive(Clone, Hash)]
struct Seed {
    generation: u64,
}

/// The supervisor: spawns a host actor per host, routes [`AppCmd`]s to the
/// right actor, and forwards every [`UiEvent`] to the app.
fn supervisor(_seed: &Seed) -> BoxStream<'static, Message> {
    iced::stream::channel(
        256,
        move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            let (appcmd_tx, mut appcmd_rx) = unbounded_channel::<AppCmd>();
            let (ui_tx, mut ui_rx) = unbounded_channel::<UiEvent>();
            let mut hosts: HashMap<HostId, HostHandle> = HashMap::new();
            let mut active: Option<HostId> = None;

            spawn_host(LOCAL_ID, HostKind::Local, &ui_tx, &mut hosts);
            if output.send(Message::Supervisor(appcmd_tx)).await.is_err() {
                return;
            }

            loop {
                tokio::select! {
                    cmd = appcmd_rx.recv() => {
                        let Some(cmd) = cmd else { break };
                        route(cmd, &ui_tx, &mut hosts, &mut active);
                    }
                    ev = ui_rx.recv() => {
                        let Some(ev) = ev else { break };
                        if output.send(Message::Ui(ev)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        },
    )
    .boxed()
}

fn route(
    cmd: AppCmd,
    ui_tx: &UnboundedSender<UiEvent>,
    hosts: &mut HashMap<HostId, HostHandle>,
    active: &mut Option<HostId>,
) {
    match cmd {
        AppCmd::AddRemote { id, spec } => {
            if !hosts.contains_key(&id) {
                spawn_host(id, HostKind::Ssh(spec), ui_tx, hosts);
            }
        }
        AppCmd::RemoveHost { id } => {
            if let Some(h) = hosts.remove(&id) {
                let _ = h.cmd_tx.send(HostCmd::Shutdown);
            }
            if *active == Some(id) {
                *active = None;
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
        AppCmd::Key(ev) => {
            if let Some(a) = *active
                && let Some(h) = hosts.get(&a)
            {
                let _ = h.cmd_tx.send(HostCmd::Key(ev));
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
        AppCmd::Scroll(lines) => {
            if let Some(a) = *active
                && let Some(h) = hosts.get(&a)
            {
                let _ = h.cmd_tx.send(HostCmd::Scroll(lines));
            }
        }
    }
}

fn spawn_host(
    id: HostId,
    kind: HostKind,
    ui_tx: &UnboundedSender<UiEvent>,
    hosts: &mut HashMap<HostId, HostHandle>,
) {
    let (cmd_tx, cmd_rx): (UnboundedSender<HostCmd>, UnboundedReceiver<HostCmd>) =
        unbounded_channel();
    let ev = ui_tx.clone();
    std::thread::spawn(move || conn::run_host(id, kind, cmd_rx, ev));
    hosts.insert(id, HostHandle { cmd_tx });
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
