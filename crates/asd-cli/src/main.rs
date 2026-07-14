//! `asd`: debug client + stdio proxy (spec §3, M0 step 5).

mod attach;
mod client;

use std::path::PathBuf;

use anyhow::bail;
use asd_proto::{ClientKind, Frame, paths};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "asd", version, about = "asd terminal mux client")]
struct Args {
    /// UDS path (defaults to $ASD_SOCKET, then $XDG_RUNTIME_DIR/asd.sock)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
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
    /// Attach to a session (detach key: Ctrl-\)
    Attach {
        name: String,
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
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    // The daemon owns its own tokio runtime, so dispatch it before entering
    // the client's #[tokio::main] runtime (no nesting).
    if matches!(args.cmd, Cmd::Daemon) {
        return asd_daemon::run(args.socket);
    }
    client_main(args)
}

#[tokio::main]
async fn client_main(args: Args) -> anyhow::Result<()> {
    let socket = args.socket.unwrap_or_else(paths::socket_path);

    match args.cmd {
        Cmd::List => {
            let mut c = client::connect(&socket, ClientKind::Cli).await?;
            c.writer.write_frame(&Frame::ListSessions).await?;
            match c.reader.read_frame().await? {
                Some(Frame::SessionList { sessions }) => {
                    if sessions.is_empty() {
                        println!("no sessions");
                    } else {
                        println!(
                            "{:<20} {:>8} {:>9} {:>13}",
                            "NAME", "SIZE", "CLIENTS", "CREATED"
                        );
                        for s in sessions {
                            println!(
                                "{:<20} {:>8} {:>9} {:>13}",
                                s.name,
                                format!("{}x{}", s.cols, s.rows),
                                s.attached_clients,
                                format_age(s.created_ms),
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
        Cmd::Daemon => unreachable!("dispatched in main before the runtime starts"),
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
