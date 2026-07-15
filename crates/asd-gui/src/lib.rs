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
mod key;
mod model;
mod render;
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
use iced::{Element, Size, Subscription, Task};
use model::{HostId, HostKind, HostState, LOCAL_ID, Model, RemoteSpec};
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
}

/// State of the *active* session's stream (the terminal pane). When nothing is
/// selected the pane shows a hint regardless of this.
pub(crate) enum Status {
    Live,
    Ended(String),
    Disconnected(String),
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
    pub(crate) remote_input: String,
    pub(crate) now_ms: u64,
    sup_tx: Option<UnboundedSender<AppCmd>>,
    generation: u64,
    /// A session named on the command line (`asd gui <session>`) to auto-select
    /// once the local host's list arrives; cleared after it is honored.
    preferred: Option<String>,
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
    RemoteInput(String),
    RemoteSubmit,
    /// Restart every host connection (the local daemon can't be auto-spawned
    /// from here, so this is how the user recovers after starting it).
    Reconnect,
    Keyboard(iced::keyboard::Event),
    Resized(Size),
    Tick,
}

impl App {
    fn new(preferred: Option<String>) -> (Self, Task<Message>) {
        let metrics = render::Metrics::new(15.0);
        // iced doesn't emit a resize on startup, so seed the grid from the
        // default window minus the chrome — otherwise the first attach sizes to
        // a stale 80×24 until the user resizes.
        let window = Size::new(960.0, 600.0);
        let w = (window.width - view::SIDEBAR_W).max(1.0);
        let h = (window.height - view::STATUS_H - view::TERMHEAD_H).max(1.0);
        let (live_cols, live_rows) = metrics.grid(Size::new(w, h));
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
                remote_input: String::new(),
                now_ms: now_ms(),
                sup_tx: None,
                generation: 0,
                preferred,
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
        self.frame = None;
        self.cache.clear();
        self.send(AppCmd::SetActive {
            host,
            name,
            cols,
            rows,
        });
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
            Message::Ui(UiEvent::Frame { host, snap }) => {
                if self.model.active.as_ref().is_some_and(|(h, _)| *h == host) {
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
            Message::RemoteInput(s) => self.remote_input = s,
            Message::RemoteSubmit => {
                let user = std::env::var("USER").unwrap_or_else(|_| "root".into());
                if let Some(spec) = RemoteSpec::parse(&self.remote_input, &user) {
                    let id = self.model.add_remote(spec.clone());
                    self.send(AppCmd::AddRemote { id, spec });
                    self.remote_input.clear();
                }
            }
            Message::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                if self.model.active.is_some()
                    && let Some(ev) = key::map_key(&key, modifiers)
                {
                    self.send(AppCmd::Key(ev));
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
        view::view(self)
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
