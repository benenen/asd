//! `asd` terminal-mux client + stdio proxy + embedded daemon, shipped as a
//! library so the single `asd` binary can drive it behind the `local` feature.
//! [`run`] parses the CLI and dispatches; a `None`/`gui` invocation is handed
//! to the caller-provided [`GuiLauncher`] (the GUI lives in a separate crate to
//! keep this one free of iced).

mod attach;
mod card;
mod client;
mod control;
mod exit;
mod platform;
mod render;

use std::path::PathBuf;

use anyhow::{Context, bail};
use asd_proto::{ClientKind, Frame, paths};
use clap::{Parser, Subcommand};

pub use exit::status as exit_status;

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
    List {
        /// Emit a JSON array instead of the table (`[]` when there are none)
        #[arg(long)]
        json: bool,
    },
    /// Create a session (auto-assigns s0, s1, ... when unnamed);
    /// starts the daemon if it is not running
    New {
        /// Session name, [A-Za-z0-9_-]{1,64}
        name: Option<String>,
        /// Command to run (parsed via sh -c); defaults to $SHELL
        #[arg(long)]
        cmd: Option<String>,
        /// Directory to start in; defaults to the daemon's. Prefer this over
        /// folding a `cd` into --cmd: the session starts there, so its recorded
        /// workspace is right from the first moment.
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Kill a session (SIGHUP, with SIGKILL fallback after 2s)
    #[command(group(clap::ArgGroup::new("kill_target").required(true).args(["name", "all"])))]
    Kill {
        /// Session name
        name: Option<String>,
        /// Kill every session instead of one
        #[arg(long)]
        all: bool,
    },
    /// Rename a session. The running program and its screen are untouched — only
    /// the name changes, so a session created with an auto-generated or prefixed
    /// name can be corrected without losing what is running in it
    Rename {
        /// Current session name
        name: String,
        /// New session name, [A-Za-z0-9_-]{1,64}
        new_name: String,
    },
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
        /// Append Enter (carriage return) after everything else. A line ending
        /// the payload already had (`echo` adds one) folds into it, so the
        /// session sees one keypress rather than a line break and then Enter
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
        /// Include history above the screen: every retained line, or at most
        /// LINES of it (`--scrollback` / `--scrollback 200`)
        #[arg(long, value_name = "LINES")]
        scrollback: Option<Option<u32>>,
        /// Emit a JSON object instead of raw text
        #[arg(long)]
        json: bool,
    },
    /// What each session is working on: the project documents in its working
    /// directory, so an agent can tell the sessions apart before running
    /// something in one. Local daemon only — the directory is read from the
    /// session's own process.
    Card {
        #[command(subcommand)]
        cmd: Option<CardCmd>,
    },
    /// Show detailed information about one session (metadata + live terminal
    /// state: pid, alt-screen, scrollback, mouse tracking, cursor)
    Inspect {
        /// Session name
        name: String,
        /// Emit a JSON object instead of a labeled block
        #[arg(long)]
        json: bool,
    },
    /// Stream a session's output as it is produced, returning once the session
    /// settles (4 on timeout, 3 if there is no such session). `wait --idle`
    /// with the output kept instead of discarded.
    Follow {
        /// Session name
        name: String,
        /// Keep streaming across quiet spells; stop only when the session ends
        /// (or --timeout expires). Without it, follow returns the first time
        /// the session has been quiet for 2 seconds.
        #[arg(long)]
        forever: bool,
        /// Give up and exit 4 after this long (500ms, 2s, 1m, 4h, 1d).
        /// Omitted, `follow` streams until the session settles or ends.
        #[arg(long)]
        timeout: Option<String>,
        /// Emit JSONL instead of raw bytes: one event object per line
        /// ({"event":"output"|"screen"|"status"|"exit"|"timeout", ...}).
        /// `output` is text that scrolled off the screen and can no longer
        /// change; `screen` is the live screen at each pause — so a repainting
        /// TUI is reported once, not once per frame.
        #[arg(long)]
        json: bool,
        /// Report the verbatim pty stream in --json instead of modelling the
        /// screen: every byte, escape sequences and repaints included. Without
        /// --json the stream is always verbatim.
        #[arg(long, requires = "json")]
        raw: bool,
    },
    /// Block until the session's screen matches or its output settles, then
    /// exit 0 (4 on timeout, 3 if there is no such session). Replaces
    /// sleep-and-poll loops in scripts.
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
    /// binary. Handy after a rebuild bumps the protocol version. Sessions are
    /// recreated from the persisted list — each as a fresh shell in its saved
    /// directory — so the names and workspaces survive, but the running
    /// programs and screen contents do not.
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

#[derive(Subcommand, Debug, Clone)]
enum CardCmd {
    /// One line per session: where it is and which documents it has (the
    /// default when `asd card` is run with no subcommand)
    List {
        /// Emit a JSON array instead of the table
        #[arg(long)]
        json: bool,
    },
    /// One session's card: its metadata plus each document's heading and
    /// opening lines
    Inspect {
        /// Session name
        name: String,
        /// Emit a JSON object instead of a labeled block
        #[arg(long)]
        json: bool,
    },
    /// Print a file from the session's working directory. The path is relative
    /// to it and may not leave it.
    Cat {
        /// Session name
        name: String,
        /// Path relative to the session's working directory
        path: String,
        /// Wrap the content in a JSON object instead of printing it raw
        #[arg(long)]
        json: bool,
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
        Cmd::List { json } => {
            let mut c = client::connect(&socket, ClientKind::Cli).await?;
            c.writer.write_frame(&Frame::ListSessions).await?;
            match c.reader.read_frame().await? {
                Some(Frame::SessionList { sessions }) => {
                    if json {
                        println!("{}", control::sessions_json(&sessions));
                    } else if sessions.is_empty() {
                        println!("no sessions");
                    } else {
                        // TITLE holds the session's own terminal title (OSC
                        // 0/2) — what a TUI says it *is*, where COMMAND only
                        // names the foreground binary. The column is sized to
                        // the widest title on screen so short titles don't
                        // push COMMAND off the terminal.
                        let titles: Vec<String> =
                            sessions.iter().map(|s| clean_title(&s.title)).collect();
                        let tw = title_col_width(&titles);
                        println!(
                            "{:<16} {:>8} {:>8} {:>8} {:>12}  {}  COMMAND",
                            "NAME",
                            "SIZE",
                            "STATUS",
                            "CLIENTS",
                            "CREATED",
                            pad_cell("TITLE", tw),
                        );
                        for (s, title) in sessions.iter().zip(&titles) {
                            println!(
                                "{:<16} {:>8} {:>8} {:>8} {:>12}  {}  {}",
                                s.name,
                                format!("{}x{}", s.cols, s.rows),
                                if s.running { "running" } else { "idle" },
                                s.attached_clients,
                                format_age(s.created_ms),
                                pad_cell(title, tw),
                                s.command,
                            );
                        }
                    }
                }
                Some(Frame::Error { code, msg }) => return Err(exit::daemon("daemon", code, &msg)),
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Cmd::New { name, cmd, cwd } => {
            // Creating a session implies wanting a daemon (tmux-like semantics)
            let mut c = client::connect_or_spawn(&socket, ClientKind::Cli).await?;
            // Resolve here so a relative --cwd means "relative to where the user
            // ran asd", not to wherever the daemon happens to be.
            let cwd = match cwd {
                Some(p) => Some(
                    std::fs::canonicalize(&p)
                        .with_context(|| format!("resolving --cwd {}", p.display()))?
                        .to_string_lossy()
                        .into_owned(),
                ),
                None => None,
            };
            c.writer
                .write_frame(&Frame::Create { name, cmd, cwd })
                .await?;
            match c.reader.read_frame().await? {
                Some(Frame::Created { name }) => println!("{name}"),
                Some(Frame::Error { code, msg }) => return Err(exit::daemon("create", code, &msg)),
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Cmd::Kill { name, all } => {
            let mut c = client::connect(&socket, ClientKind::Cli).await?;
            // clap's group guarantees exactly one of name / --all.
            let names = match name {
                Some(n) => vec![n],
                None => {
                    debug_assert!(all);
                    c.writer.write_frame(&Frame::ListSessions).await?;
                    match c.reader.read_frame().await? {
                        Some(Frame::SessionList { sessions }) => {
                            sessions.into_iter().map(|s| s.name).collect()
                        }
                        Some(Frame::Error { code, msg }) => {
                            return Err(exit::daemon("kill", code, &msg));
                        }
                        other => bail!("unexpected reply: {other:?}"),
                    }
                }
            };
            if names.is_empty() {
                println!("no sessions");
                return Ok(());
            }
            for n in &names {
                c.writer
                    .write_frame(&Frame::Kill { name: n.clone() })
                    .await?;
            }
            // Kill has no ack frame (spec §4): use a ListSessions to anchor
            // the confirmation ordering — the daemon processes in order, so
            // any Kill error arrives before the SessionList.
            c.writer.write_frame(&Frame::ListSessions).await?;
            loop {
                match c.reader.read_frame().await? {
                    Some(Frame::SessionList { .. }) => break,
                    // Killing several at once races their own exits; one that
                    // died on its own in the meantime is the outcome we wanted,
                    // not a failure.
                    Some(Frame::Error { code, msg }) => {
                        if code == asd_proto::code::NO_SUCH_SESSION && names.len() > 1 {
                            continue;
                        }
                        return Err(exit::daemon("kill", code, &msg));
                    }
                    other => bail!("unexpected reply: {other:?}"),
                }
            }
            for n in &names {
                println!("kill signalled: {n}");
            }
        }
        Cmd::Attach { name, auto, stdio } => {
            if stdio {
                // The pure byte proxy does no handshake: the pipe's far end
                // speaks the protocol.
                if auto {
                    // First make sure the daemon is alive (one handshake
                    // connection to probe/start it)
                    let _ = client::connect_or_spawn(&socket, ClientKind::Proxy).await?;
                }
                return platform::run_stdio_proxy(&socket).await;
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
                        cwd: None,
                    })
                    .await?;
                match c.reader.read_frame().await? {
                    Some(Frame::Created { .. }) => {}
                    Some(Frame::Error { code, msg }) if code == asd_proto::code::SESSION_EXISTS => {
                        // Colliding with a concurrent create is fine, as long
                        // as we can attach
                        let _ = msg;
                    }
                    Some(Frame::Error { code, msg }) => {
                        return Err(exit::daemon("create", code, &msg));
                    }
                    other => bail!("unexpected reply: {other:?}"),
                }
            }

            attach::run(c, &name).await?;
        }
        Cmd::Rename { name, new_name } => control::rename(&socket, name, new_name).await?,
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
        } => control::peek(&socket, name, control::scrollback_arg(scrollback), json).await?,
        Cmd::Card { cmd } => match cmd.unwrap_or(CardCmd::List { json: false }) {
            CardCmd::List { json } => card::list(&socket, json).await?,
            CardCmd::Inspect { name, json } => card::inspect(&socket, name, json).await?,
            CardCmd::Cat { name, path, json } => card::cat(&socket, name, path, json).await?,
        },
        Cmd::Inspect { name, json } => control::inspect(&socket, name, json).await?,
        Cmd::Follow {
            name,
            forever,
            timeout,
            json,
            raw,
        } => control::follow(&socket, name, !forever, timeout, json, raw).await?,
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
        Some(Frame::Error { code, msg }) => Err(exit::daemon("daemon", code, &msg)),
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// Widest the TITLE column may grow, in display columns. NAME..CREATED
/// already take 58, so this keeps a titled table inside ~100 columns.
const TITLE_COL_MAX: usize = 32;

/// A session's terminal title as table text: control characters dropped (a
/// rogue OSC title must not break the table or move the caller's cursor) and
/// surrounding whitespace trimmed.
pub(crate) fn clean_title(title: &str) -> String {
    title
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

/// Width of the TITLE column: the widest title present, never below the
/// header's own width and never past [`TITLE_COL_MAX`].
pub(crate) fn title_col_width(titles: &[String]) -> usize {
    titles
        .iter()
        .map(|t| str_width(t))
        .max()
        .unwrap_or(0)
        .clamp("TITLE".len(), TITLE_COL_MAX)
}

/// Fit `s` into exactly `width` display columns: truncated with an ellipsis
/// when too wide, space-padded when too narrow. Padding is by display width,
/// not char count, so a CJK title doesn't shove COMMAND out of line.
pub(crate) fn pad_cell(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    if str_width(s) > width {
        // Reserve one column for the ellipsis; add whole chars while they fit.
        for c in s.chars() {
            let cw = str_width(c.encode_utf8(&mut [0u8; 4]));
            if w + cw > width.saturating_sub(1) {
                break;
            }
            out.push(c);
            w += cw;
        }
        if width > 0 {
            out.push('…');
            w += 1;
        }
    } else {
        out.push_str(s);
        w = str_width(s);
    }
    out.extend(std::iter::repeat_n(' ', width - w));
    out
}

/// Display width of a string in terminal cells (CJK glyphs are 2 wide).
fn str_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

pub(crate) fn format_age(created_ms: u64) -> String {
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

#[cfg(test)]
mod tests {
    use super::{TITLE_COL_MAX, clean_title, pad_cell, str_width, title_col_width};

    #[test]
    fn clean_title_trims_and_strips_control_characters() {
        assert_eq!(clean_title("  Claude Code  "), "Claude Code");
        assert_eq!(clean_title(""), "");
        // An OSC title carrying escapes must not repaint the caller's screen.
        assert_eq!(
            clean_title("vim\x1b[2Jsrc/main.rs\nx"),
            "vim[2Jsrc/main.rsx"
        );
    }

    #[test]
    fn title_col_width_fits_the_titles_within_bounds() {
        // Never narrower than the header, even with no titles at all.
        assert_eq!(title_col_width(&[]), 5);
        assert_eq!(title_col_width(&["ab".to_string()]), 5);
        // Sized to the widest title present...
        assert_eq!(
            title_col_width(&["short".to_string(), "a longer title".to_string()]),
            14
        );
        // ...but capped, so COMMAND stays on screen.
        assert_eq!(title_col_width(&["x".repeat(200)]), TITLE_COL_MAX);
    }

    #[test]
    fn pad_cell_produces_exactly_the_column_width() {
        assert_eq!(pad_cell("ab", 5), "ab   ");
        assert_eq!(pad_cell("abcde", 5), "abcde");
        assert_eq!(pad_cell("abcdefg", 5), "abcd…");
        // CJK glyphs are 2 columns each; a wide glyph is never split, and the
        // cell still measures exactly `width` so COMMAND stays aligned.
        assert_eq!(str_width(&pad_cell("中文标题", 5)), 5);
        assert_eq!(pad_cell("中文标题", 5), "中文…");
        for w in 1..=10 {
            assert_eq!(str_width(&pad_cell("中文标题abc", w)), w, "width {w}");
        }
    }
}
