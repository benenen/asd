//! `asd-gui`: the GPU terminal client (spec §7).
//!
//! One window = one UDS connection + a render thread that owns the render
//! terminal (see [`conn`]). The thread streams [`RenderSnapshot`]s to this iced
//! app, which draws them on a canvas; key presses go back to the thread to be
//! encoded (its mode state mirrors the session, so DECCKM etc. stay in sync).

mod conn;
mod key;
mod render;

use std::path::PathBuf;

use asd_proto::paths;
use asd_vt::RenderSnapshot;
use conn::{Cmd, WorkerEvent};
use iced::futures::stream::BoxStream;
use iced::futures::{SinkExt, StreamExt};
use iced::widget::{Canvas, canvas, center, text};
use iced::{Element, Length, Size, Subscription, Task};
use tokio::sync::mpsc::UnboundedSender;

fn main() -> iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    iced::application(App::new, App::update, App::view)
        .subscription(App::subscription)
        .title(App::title)
        .antialiasing(true)
        .run()
}

/// Identity of a connection attempt. Its `Hash` drives the subscription: bump
/// `generation` and iced restarts the worker — that is how reconnect works.
#[derive(Clone, Hash)]
struct ConnConfig {
    socket: PathBuf,
    name: String,
    cols: u16,
    rows: u16,
    generation: u64,
}

enum Status {
    Connecting,
    Live,
    Ended(String),
    Disconnected(String),
}

struct App {
    config: ConnConfig,
    cmd_tx: Option<UnboundedSender<Cmd>>,
    frame: Option<RenderSnapshot>,
    cache: canvas::Cache,
    status: Status,
    metrics: render::Metrics,
    live_cols: u16,
    live_rows: u16,
}

#[derive(Debug, Clone)]
enum Message {
    /// The worker started; carries the channel to send it commands.
    Connected(UnboundedSender<Cmd>),
    Worker(WorkerEvent),
    Keyboard(iced::keyboard::Event),
    Resized(Size),
}

impl App {
    fn new() -> (Self, Task<Message>) {
        // `asd-gui [session]` — session name defaults to s0; socket honors
        // $ASD_SOCKET via the shared path contract.
        let name = std::env::args().nth(1).unwrap_or_else(|| "s0".to_string());
        let metrics = render::Metrics::new(15.0);
        let (cols, rows) = (80u16, 24u16);
        let config = ConnConfig {
            socket: paths::socket_path(),
            name,
            cols,
            rows,
            generation: 0,
        };
        (
            Self {
                config,
                cmd_tx: None,
                frame: None,
                cache: canvas::Cache::new(),
                status: Status::Connecting,
                metrics,
                live_cols: cols,
                live_rows: rows,
            },
            Task::none(),
        )
    }

    fn title(&self) -> String {
        format!("asd — {}", self.config.name)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Connected(tx) => self.cmd_tx = Some(tx),
            Message::Worker(WorkerEvent::Frame(snap)) => {
                self.frame = Some(*snap);
                self.cache.clear();
                self.status = Status::Live;
            }
            Message::Worker(WorkerEvent::SessionEnded(msg)) => {
                self.status = Status::Ended(msg);
                self.cmd_tx = None;
            }
            Message::Worker(WorkerEvent::Disconnected(msg)) => {
                self.status = Status::Disconnected(msg);
                self.cmd_tx = None;
            }
            Message::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                // While disconnected, 'r' reconnects; other keys do nothing.
                if matches!(self.status, Status::Disconnected(_) | Status::Ended(_)) {
                    if let iced::keyboard::Key::Character(s) = &key
                        && s.as_str() == "r"
                    {
                        return self.reconnect();
                    }
                    return Task::none();
                }
                if let Some(ev) = key::map_key(&key, modifiers)
                    && let Some(tx) = &self.cmd_tx
                {
                    let _ = tx.send(Cmd::Key(ev));
                }
            }
            // Key releases and modifier changes are not forwarded.
            Message::Keyboard(_) => {}
            Message::Resized(size) => {
                let (cols, rows) = self.metrics.grid(size);
                if (cols, rows) != (self.live_cols, self.live_rows) {
                    self.live_cols = cols;
                    self.live_rows = rows;
                    if let Some(tx) = &self.cmd_tx {
                        let _ = tx.send(Cmd::Resize { cols, rows });
                    }
                }
            }
        }
        Task::none()
    }

    fn reconnect(&mut self) -> Task<Message> {
        // Bumping the generation changes ConnConfig's hash → the subscription
        // restarts with a fresh connection.
        self.config.generation += 1;
        self.config.cols = self.live_cols;
        self.config.rows = self.live_rows;
        self.status = Status::Connecting;
        self.frame = None;
        self.cmd_tx = None;
        self.cache.clear();
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            iced::keyboard::listen().map(Message::Keyboard),
            iced::window::resize_events().map(|(_, size)| Message::Resized(size)),
        ];
        // A dead session does not auto-retry; everything else keeps the worker
        // subscription registered (the same hash won't restart a finished
        // stream — only a generation bump does).
        if !matches!(self.status, Status::Ended(_)) {
            subs.push(Subscription::run_with(
                self.config.clone(),
                connection_worker,
            ));
        }
        Subscription::batch(subs)
    }

    fn view(&self) -> Element<'_, Message> {
        match &self.status {
            Status::Live | Status::Connecting => Canvas::new(render::TermCanvas {
                frame: self.frame.as_ref(),
                cache: &self.cache,
                metrics: self.metrics,
            })
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
            Status::Disconnected(msg) => center(text(format!(
                "[disconnected: {msg}]  —  press r to reconnect"
            )))
            .into(),
            Status::Ended(msg) => center(text(format!(
                "[session ended: {msg}]  —  press r to reconnect"
            )))
            .into(),
        }
    }
}

/// Subscription worker: spawns the render thread and bridges its channels to
/// iced messages. `fn(&ConnConfig)` so it can be a plain function pointer for
/// `Subscription::run_with`.
fn connection_worker(cfg: &ConnConfig) -> BoxStream<'static, Message> {
    let cfg = cfg.clone();
    iced::stream::channel(
        256,
        move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
            let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<WorkerEvent>();
            std::thread::spawn(move || {
                conn::run(cfg.socket, cfg.name, cfg.cols, cfg.rows, cmd_rx, ev_tx);
            });
            let _ = output.send(Message::Connected(cmd_tx)).await;
            while let Some(ev) = ev_rx.recv().await {
                let terminal = !matches!(ev, WorkerEvent::Frame(_));
                let _ = output.send(Message::Worker(ev)).await;
                if terminal {
                    break;
                }
            }
        },
    )
    .boxed()
}
