//! Interactive attach as a VT-render client, plus the `--stdio` proxy.
//!
//! Unlike a dumb passthrough, this client keeps its own [`GhosttyVt`] terminal
//! (fed the daemon's Snapshot + Output) and renders it to the screen itself.
//! That local VT model is what lets a single Live view do all three at once:
//! - **detach restores the original screen** (we run on the alternate screen);
//! - **wheel scrolls back through history** (we scroll our own viewport — a
//!   client-local action that never disturbs other attached clients);
//! - **drag-select copies** (we highlight the selection and write the system
//!   clipboard via OSC 52, so the terminal's own selection is not needed and
//!   mouse reporting can stay on to catch the wheel).
//!
//! When the session program is itself full-screen or grabbing the mouse (vim,
//! htop), the wheel and clicks are forwarded to it instead — the client knows
//! precisely from its VT model (`is_alt_screen` / `is_mouse_tracking`).

use std::io::Write as _;
use std::os::fd::AsFd;

use anyhow::Context;
use asd_proto::Frame;
use asd_vt::{GhosttyVt, VtBackend};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::client::Client;
use crate::render::{self, MouseKind, Selection};

/// Detach key: Ctrl-\ (byte 0x1C in raw mode).
const DETACH_BYTE: u8 = 0x1c;
/// Lines moved per wheel tick.
const WHEEL_LINES: usize = 3;

/// Frames the socket-reader task forwards to the main loop.
enum Ev {
    Output(Vec<u8>),
    Snapshot(Vec<u8>),
    Ended(Exit),
}

enum Exit {
    Detached,
    SessionEnded(String),
    DaemonGone,
}

pub async fn run(mut client: Client, name: &str) -> anyhow::Result<()> {
    let (mut cols, mut rows) = term_size();
    client
        .writer
        .write_frame(&Frame::Attach {
            name: name.to_string(),
            cols,
            rows,
        })
        .await?;

    // The first frame must be Snapshot (or an error); handle errors before
    // switching the terminal's mode so the message stays visible.
    let first = match client.reader.read_frame().await? {
        Some(Frame::Snapshot { vt }) => vt,
        Some(Frame::Error { code, msg }) => anyhow::bail!("attach failed ({code}): {msg}"),
        other => anyhow::bail!("expected Snapshot after Attach, got {other:?}"),
    };

    eprintln!("[asd: attached to '{name}', detach: Ctrl-\\]");
    let _raw = RawGuard::enable().context("enabling raw terminal mode")?;
    // Alt screen (detach restores the caller's screen) + mouse reporting (so
    // the wheel and drags reach us as events). Dropped before RawGuard.
    let _screen = ScreenGuard::enter().context("entering alternate screen")?;

    // Our local mirror of the session terminal.
    let mut vt = GhosttyVt::new(cols.max(1), rows.max(1), 100_000);
    vt.feed(&first);
    let _ = vt.take_pty_responses(); // the daemon already answered any queries

    // Scrollback view state.
    let mut scroll = 0usize; // lines scrolled up from the live bottom
    let mut selection: Option<Selection> = None;
    let mut selecting = false;

    render_now(&mut vt, scroll, selection)?;

    // Socket reader → Ev channel.
    let (ev_tx, mut ev_rx) = mpsc::channel::<Ev>(256);
    let mut reader = client.reader;
    let reader_task = tokio::spawn(async move {
        loop {
            let ev = match reader.read_frame().await {
                Ok(Some(Frame::Output { bytes })) => Ev::Output(bytes),
                Ok(Some(Frame::Snapshot { vt })) => Ev::Snapshot(vt),
                Ok(Some(Frame::Error { code, msg })) => Ev::Ended(Exit::SessionEnded(
                    if code == asd_proto::code::SESSION_EXITED {
                        msg
                    } else {
                        format!("error {code}: {msg}")
                    },
                )),
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => Ev::Ended(Exit::DaemonGone),
            };
            let stop = matches!(ev, Ev::Ended(_));
            if ev_tx.send(ev).await.is_err() || stop {
                break;
            }
        }
    });

    // Stdin reader → raw byte chunks; None on EOF.
    let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(64);
    let stdin_task = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 8192];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if in_tx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut sigwinch = signal(SignalKind::window_change())?;
    let mut writer = client.writer;

    let exit = loop {
        tokio::select! {
            ev = ev_rx.recv() => {
                let Some(ev) = ev else { break Exit::DaemonGone };
                match ev {
                    Ev::Ended(reason) => break reason,
                    Ev::Output(bytes) => {
                        vt.feed(&bytes);
                        let _ = vt.take_pty_responses();
                        // Drain any further pending output before painting once.
                        while let Ok(Ev::Output(more)) = ev_rx.try_recv() {
                            vt.feed(&more);
                            let _ = vt.take_pty_responses();
                        }
                        // While scrolled up we keep the view frozen so reading
                        // history is not yanked around by live output.
                        if scroll == 0 && render_now(&mut vt, scroll, selection).is_err() {
                            break Exit::DaemonGone;
                        }
                    }
                    Ev::Snapshot(dump) => {
                        vt.feed(&dump);
                        let _ = vt.take_pty_responses();
                        if render_now(&mut vt, scroll, selection).is_err() {
                            break Exit::DaemonGone;
                        }
                    }
                }
            }
            chunk = in_rx.recv() => {
                let Some(chunk) = chunk else { break Exit::Detached };
                if chunk.contains(&DETACH_BYTE) {
                    let _ = writer.write_frame(&Frame::Detach).await;
                    break Exit::Detached;
                }

                // If the app is full-screen or grabbing the mouse, everything
                // (including the wheel) belongs to it.
                let app_owns_mouse = vt.is_alt_screen() || vt.is_mouse_tracking();
                let mouse = render::parse_mouse(&chunk);

                if app_owns_mouse || mouse.is_empty() {
                    // Typing (or app-owned mouse): forward verbatim. Typing
                    // snaps the view back to the live bottom.
                    if scroll != 0 && !is_only_mouse(&chunk) {
                        scroll = 0;
                        selection = None;
                        let _ = render_now(&mut vt, scroll, selection);
                    }
                    if writer.write_frame(&Frame::Input { bytes: chunk }).await.is_err() {
                        break Exit::DaemonGone;
                    }
                    continue;
                }

                // Shell prompt: the wheel scrolls our view, drags select+copy.
                let mut dirty = false;
                let max_scroll = vt.scrollback_rows();
                for ev in mouse {
                    match ev.kind {
                        MouseKind::WheelUp => {
                            scroll = (scroll + WHEEL_LINES).min(max_scroll);
                            dirty = true;
                        }
                        MouseKind::WheelDown => {
                            scroll = scroll.saturating_sub(WHEEL_LINES);
                            dirty = true;
                        }
                        MouseKind::Press => {
                            selecting = true;
                            selection = Some(Selection { start: (ev.x, ev.y), end: (ev.x, ev.y) });
                            dirty = true;
                        }
                        MouseKind::Drag => {
                            if selecting && let Some(sel) = &mut selection {
                                sel.end = (ev.x, ev.y);
                                dirty = true;
                            }
                        }
                        MouseKind::Release => {
                            if selecting && let Some(sel) = selection {
                                selecting = false;
                                // Viewport is already positioned at `scroll`.
                                let text = vt.selection_text(asd_vt::Selection {
                                    start: sel.start,
                                    end: sel.end,
                                    block: false,
                                });
                                if !text.is_empty() {
                                    let _ = write_stdout(&render::osc52_copy(&text));
                                }
                            }
                        }
                        MouseKind::Other => {}
                    }
                }
                if dirty && render_now(&mut vt, scroll, selection).is_err() {
                    break Exit::DaemonGone;
                }
            }
            _ = sigwinch.recv() => {
                let (c, r) = term_size();
                cols = c; rows = r;
                vt.resize(cols.max(1), rows.max(1));
                if writer.write_frame(&Frame::Resize { cols, rows }).await.is_err() {
                    break Exit::DaemonGone;
                }
                let _ = render_now(&mut vt, scroll, selection);
            }
        }
    };

    reader_task.abort();
    stdin_task.abort();
    drop(_screen);
    drop(_raw);

    match exit {
        Exit::Detached => eprintln!("[asd: detached]"),
        Exit::SessionEnded(msg) => eprintln!("[asd: {msg}]"),
        Exit::DaemonGone => eprintln!("[asd: connection to daemon lost]"),
    }
    Ok(())
}

/// Position the viewport at `scroll` and paint one frame.
fn render_now(vt: &mut GhosttyVt, scroll: usize, sel: Option<Selection>) -> std::io::Result<()> {
    vt.set_scroll(scroll);
    let snap = vt.render_snapshot();
    write_stdout(&render::render_frame(&snap, sel))
}

/// Whether a chunk is entirely SGR mouse reports (so it should not be forwarded
/// as typing when the app owns the mouse... actually used to avoid snapping the
/// scroll to bottom on a stray mouse event).
fn is_only_mouse(chunk: &[u8]) -> bool {
    !chunk.is_empty() && chunk.starts_with(b"\x1b[<")
}

/// `--stdio` proxy: bidirectional raw byte copy between stdio and the UDS;
/// the protocol is spoken by the pipe's far end (a future remote GUI/CLI) —
/// this process is a pure passthrough.
/// SSH dumb-pipe scenario: `ssh host "asd attach --stdio"`.
pub async fn run_stdio_proxy(socket: &std::path::Path) -> anyhow::Result<()> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting {}", socket.display()))?;
    let (mut sock_r, mut sock_w) = stream.into_split();

    let to_sock = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let _ = tokio::io::copy(&mut stdin, &mut sock_w).await;
        let _ = sock_w.shutdown().await;
    });
    let mut stdout = tokio::io::stdout();
    let _ = tokio::io::copy(&mut sock_r, &mut stdout).await;
    let _ = stdout.flush().await;
    to_sock.abort();
    Ok(())
}

/// Synchronous stdout write (the lock's lifetime stays inside the function —
/// never across an await point).
fn write_stdout(bytes: &[u8]) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()
}

/// Terminal size; 80×24 when unavailable (not a tty).
pub fn term_size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if ret == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_col, ws.ws_row)
    } else {
        (80, 24)
    }
}

/// Alternate-screen + mouse-reporting guard. Enters the alternate screen (DEC
/// 1049), enables button + SGR mouse reporting (1000/1006) so the wheel and
/// drags arrive as events, and enables focus... on drop, undoes all of it and
/// restores the primary screen with its previous contents.
struct ScreenGuard;

impl ScreenGuard {
    fn enter() -> std::io::Result<Self> {
        write_stdout(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h")?;
        Ok(Self)
    }
}

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        let _ = write_stdout(b"\x1b[?1006l\x1b[?1000l\x1b[?1049l\x1b[0 q");
    }
}

/// Raw mode guard: restores the original termios on drop.
struct RawGuard {
    original: nix::sys::termios::Termios,
}

impl RawGuard {
    fn enable() -> anyhow::Result<Self> {
        use nix::sys::termios::{SetArg, cfmakeraw, tcgetattr, tcsetattr};
        let stdin = std::io::stdin();
        let original = tcgetattr(stdin.as_fd())?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &raw)?;
        Ok(Self { original })
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        use nix::sys::termios::{SetArg, tcsetattr};
        let stdin = std::io::stdin();
        let _ = tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &self.original);
    }
}
