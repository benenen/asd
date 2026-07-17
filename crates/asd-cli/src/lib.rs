//! `asd` terminal-mux client + stdio proxy + embedded daemon, shipped as a
//! library so the single `asd` binary can drive it behind the `local` feature.
//! [`run`] parses the CLI and dispatches; a `None`/`gui` invocation is handed
//! to the caller-provided [`GuiLauncher`] (the GUI lives in a separate crate to
//! keep this one free of iced).

mod attach;
mod client;
mod control;
mod render;

use std::path::PathBuf;

use anyhow::bail;
use asd_proto::{ClientKind, Frame, paths};
use clap::{Parser, Subcommand};

/// Launches the GUI for an optional session name. Injected by the binary (the
/// GUI crate is only linked under the `gui` feature), so this crate never
/// depends on iced.
pub type GuiLauncher = fn(Option<String>) -> anyhow::Result<()>;

#[derive(Parser, Debug)]
#[command(name = "asd", version, about = "asd terminal mux client")]
struct Args {
    /// UDS path (defaults to $ASD_SOCKET, then $XDG_RUNTIME_DIR/asd.sock)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// No subcommand opens the GUI (when built with it).
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List all sessions
    List,
    /// Create a session (auto-assigns s0, s1, ... when unnamed);
    /// starts the daemon if it is not running
    New {
        /// Session name, [A-Za-z0-9_-]{1,64}
        name: Option<String>,
        /// Command to run (parsed via sh -c); defaults to $SHELL
        #[arg(long)]
        cmd: Option<String>,
    },
    /// Kill a session (SIGHUP, with SIGKILL fallback after 2s)
    Kill { name: String },
    /// Type into a session, exactly as if typed at the keyboard. --text is sent
    /// literally (no escaping, no implicit newline); with neither --text nor
    /// --key, bytes are read from stdin (binary-safe, NUL excluded).
    Send {
        /// Session name
        name: String,
        /// The text to type (sent literally)
        #[arg(long, conflicts_with_all = ["key", "stdin"])]
        text: Option<String>,
        /// Named keys, comma-separated: Enter, Tab, Escape, Space, Backspace,
        /// Up, Down, Left, Right, Home, End, C-a..C-z
        #[arg(long, conflicts_with = "stdin")]
        key: Option<String>,
        /// Append Enter (carriage return) after everything else
        #[arg(long)]
        enter: bool,
        /// Force reading the payload from stdin
        #[arg(long)]
        stdin: bool,
    },
    /// Print the session's rendered screen (reconstructed from terminal state,
    /// not a raw byte log); safe to run while attached
    Peek {
        /// Session name
        name: String,
        /// Include the full scrollback history above the screen
        #[arg(long)]
        scrollback: bool,
        /// Emit a JSON object instead of raw text
        #[arg(long)]
        json: bool,
    },
    /// Block until the session's screen matches or its output settles, then
    /// exit 0 (exit 4 on timeout). Replaces sleep-and-poll loops in scripts.
    #[command(group(clap::ArgGroup::new("wait_cond").required(true).args(["text", "idle"])))]
    Wait {
        /// Session name
        name: String,
        /// Until the rendered screen contains this text (plain substring)
        #[arg(long)]
        text: Option<String>,
        /// Until the session has produced no output for 2 seconds
        #[arg(long)]
        idle: bool,
        /// Give up and exit 4 after this long (500ms, 2s, 1m, 4h, 1d)
        #[arg(long, default_value = "30s")]
        timeout: String,
    },
    /// Attach to a session (detach key: Ctrl-\)
    Attach {
        /// Session name; not used (and not required) with --stdio
        #[arg(required_unless_present = "stdio")]
        name: Option<String>,
        /// Self-heal: start the daemon (setsid) if absent; create the session if missing
        #[arg(short = 'A', long)]
        auto: bool,
        /// Raw byte proxy stdio ↔ UDS (for SSH dumb pipes); does not interpret the protocol
        #[arg(long)]
        stdio: bool,
    },
    /// Run the mux daemon in the foreground (normally started on demand by
    /// `asd new` / `asd attach -A`)
    Daemon,
    /// Restart the daemon: stop the running one and start a fresh copy of this
    /// binary. Handy after a rebuild bumps the protocol version (all sessions
    /// are lost — the daemon does not persist them).
    Restart,
    /// Open the GUI (same as running `asd` with no subcommand).
    Gui {
        /// Session to pre-select.
        session: Option<String>,
    },
    /// Open the terminal UI: a session sidebar next to a live terminal pane
    /// (switch with Ctrl+A; starts the daemon if it is not running)
    Ui {
        /// Session to pre-select.
        session: Option<String>,
    },
}

/// Parse the CLI and run the requested command. A `None`/`gui` invocation opens
/// the GUI via `gui` (absent in a `local`-only build). Not async: the daemon
/// and the GUI each own their own runtime, and the client commands get a
/// current-thread runtime below — so nothing nests.
pub fn run(gui: Option<GuiLauncher>) -> anyhow::Result<()> {
    let args = Args::parse();
    match &args.cmd {
        // The daemon owns its own tokio runtime → dispatch before ours starts.
        Some(Cmd::Daemon) => return asd_daemon::run(args.socket),
        // No subcommand or `gui` → hand off to the injected GUI launcher.
        None => return launch_gui(gui, None),
        Some(Cmd::Gui { session }) => return launch_gui(gui, session.clone()),
        // The TUI runs its own event loop + conn thread; keep it off the
        // client runtime as well (its session preselect rides along).
        Some(Cmd::Ui { session }) => return run_ui(args.socket, session.clone()),
        _ => {}
    }
    client_main(args)
}

/// Ensure the daemon is up (self-heal, like `attach -A`), then hand the
/// terminal to the TUI.
fn run_ui(socket: Option<PathBuf>, session: Option<String>) -> anyhow::Result<()> {
    let socket = socket.unwrap_or_else(paths::socket_path);
    // One probe connection on a scratch runtime; dropped before the TUI runs.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            client::connect_or_spawn(&socket, ClientKind::Cli)
                .await
                .map(drop)
        })?;
    asd_tui::run(socket, session)
}

fn launch_gui(gui: Option<GuiLauncher>, session: Option<String>) -> anyhow::Result<()> {
    match gui {
        Some(launch) => launch(session),
        None => bail!(
            "this build has no GUI (compiled without the `gui` feature); \
             use a subcommand such as `asd new` or `asd attach`"
        ),
    }
}

// current_thread: the render client holds a `!Send` GhosttyVt across awaits.
#[tokio::main(flavor = "current_thread")]
async fn client_main(args: Args) -> anyhow::Result<()> {
    let socket = args.socket.unwrap_or_else(paths::socket_path);

    // Daemon/Gui/None are dispatched in `run` before this runtime starts.
    let Some(cmd) = args.cmd else {
        unreachable!("no-subcommand is dispatched before the runtime starts")
    };
    match cmd {
        Cmd::List => {
            let mut c = client::connect(&socket, ClientKind::Cli).await?;
            c.writer.write_frame(&Frame::ListSessions).await?;
            match c.reader.read_frame().await? {
                Some(Frame::SessionList { sessions }) => {
                    if sessions.is_empty() {
                        println!("no sessions");
                    } else {
                        println!(
                            "{:<16} {:>8} {:>8} {:>8} {:>12}  COMMAND",
                            "NAME", "SIZE", "STATUS", "CLIENTS", "CREATED"
                        );
                        for s in sessions {
                            println!(
                                "{:<16} {:>8} {:>8} {:>8} {:>12}  {}",
                                s.name,
                                format!("{}x{}", s.cols, s.rows),
                                if s.running { "running" } else { "idle" },
                                s.attached_clients,
                                format_age(s.created_ms),
                                s.command,
                            );
                        }
                    }
                }
                Some(Frame::Error { code, msg }) => bail!("daemon error ({code}): {msg}"),
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Cmd::New { name, cmd } => {
            // Creating a session implies wanting a daemon (tmux-like semantics)
            let mut c = client::connect_or_spawn(&socket, ClientKind::Cli).await?;
            c.writer.write_frame(&Frame::Create { name, cmd }).await?;
            match c.reader.read_frame().await? {
                Some(Frame::Created { name }) => println!("{name}"),
                Some(Frame::Error { code, msg }) => bail!("create failed ({code}): {msg}"),
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Cmd::Kill { name } => {
            let mut c = client::connect(&socket, ClientKind::Cli).await?;
            c.writer
                .write_frame(&Frame::Kill { name: name.clone() })
                .await?;
            // Kill has no ack frame (spec §4): use a ListSessions to anchor
            // the confirmation ordering — the daemon processes in order, so
            // if Kill failed, the Error arrives before the SessionList.
            c.writer.write_frame(&Frame::ListSessions).await?;
            match c.reader.read_frame().await? {
                Some(Frame::SessionList { .. }) => println!("kill signalled: {name}"),
                Some(Frame::Error { code, msg }) => bail!("kill failed ({code}): {msg}"),
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Cmd::Attach { name, auto, stdio } => {
            if stdio {
                // The pure byte proxy does no handshake: the pipe's far end
                // speaks the protocol
                if auto {
                    // First make sure the daemon is alive (one handshake
                    // connection to probe/start it)
                    let _ = client::connect_or_spawn(&socket, ClientKind::Proxy).await?;
                }
                return attach::run_stdio_proxy(&socket).await;
            }
            // clap enforces NAME unless --stdio, so this cannot fail here.
            let name = name.expect("NAME is required without --stdio");

            // tmux's $TMUX idea: attaching the session this shell runs inside
            // is a render feedback loop that floods the pty for everyone.
            if std::env::var("ASD_SESSION").as_deref() == Ok(name.as_str()) {
                bail!(
                    "refusing to attach '{name}': this shell runs inside it \
                     (unset ASD_SESSION to force)"
                );
            }

            let mut c = if auto {
                client::connect_or_spawn(&socket, ClientKind::Cli).await?
            } else {
                client::connect(&socket, ClientKind::Cli).await?
            };

            // -A: create the session first if it does not exist
            // (tmux new-session -A semantics)
            if auto && !session_exists(&mut c, &name).await? {
                c.writer
                    .write_frame(&Frame::Create {
                        name: Some(name.clone()),
                        cmd: None,
                    })
                    .await?;
                match c.reader.read_frame().await? {
                    Some(Frame::Created { .. }) => {}
                    Some(Frame::Error { code, msg }) if code == asd_proto::code::SESSION_EXISTS => {
                        // Colliding with a concurrent create is fine, as long
                        // as we can attach
                        let _ = msg;
                    }
                    Some(Frame::Error { code, msg }) => bail!("create failed ({code}): {msg}"),
                    other => bail!("unexpected reply: {other:?}"),
                }
            }

            attach::run(c, &name).await?;
        }
        Cmd::Send {
            name,
            text,
            key,
            enter,
            stdin,
        } => control::send(&socket, name, text, key, enter, stdin).await?,
        Cmd::Peek {
            name,
            scrollback,
            json,
        } => control::peek(&socket, name, scrollback, json).await?,
        Cmd::Wait {
            name,
            text,
            idle,
            timeout,
        } => control::wait(&socket, name, text, idle, timeout).await?,
        Cmd::Restart => {
            let c = client::restart(&socket).await?;
            println!(
                "asd-daemon restarted (v{}, proto v{})",
                c.daemon_version,
                asd_proto::PROTO_VERSION
            );
        }
        Cmd::Daemon | Cmd::Gui { .. } | Cmd::Ui { .. } => {
            unreachable!("dispatched in `run` before the runtime starts")
        }
    }
    Ok(())
}

async fn session_exists(c: &mut client::Client, name: &str) -> anyhow::Result<bool> {
    c.writer.write_frame(&Frame::ListSessions).await?;
    match c.reader.read_frame().await? {
        Some(Frame::SessionList { sessions }) => Ok(sessions.iter().any(|s| s.name == name)),
        Some(Frame::Error { code, msg }) => bail!("daemon error ({code}): {msg}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

fn format_age(created_ms: u64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let secs = now_ms.saturating_sub(created_ms) / 1000;
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}
