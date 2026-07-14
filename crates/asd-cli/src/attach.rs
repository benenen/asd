//! Interactive attach as a VT-render client, plus the `--stdio` proxy.
//!
//! Unlike a dumb passthrough, this client keeps its own [`GhosttyVt`] terminal
//! (fed the daemon's Snapshot + Output) and renders it to the screen itself.
//! That local VT model gives a single Live view two things at once:
//! - **detach restores the original screen** (we run on the alternate screen);
//! - **scrollback into history** — a client-local action (Shift+PageUp/PageDown/
//!   Home/End) that never disturbs other attached clients.
//!
//! We deliberately do **not** grab the mouse: no mouse-tracking modes are
//! enabled, so the host terminal's own drag-to-select and copy keep working.
//! (On the alternate screen the host's native scrollback is unavailable, which
//! is why scrollback is driven from the keyboard here.)

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
use crate::render;

/// Detach key: Ctrl-\ (byte 0x1C in raw mode).
const DETACH_BYTE: u8 = 0x1c;

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

    // Lines scrolled up from the live bottom (0 = following live output).
    let mut scroll = 0usize;

    render_now(&mut vt, scroll)?;

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
                        if scroll == 0 && render_now(&mut vt, scroll).is_err() {
                            break Exit::DaemonGone;
                        }
                    }
                    Ev::Snapshot(dump) => {
                        vt.feed(&dump);
                        let _ = vt.take_pty_responses();
                        if render_now(&mut vt, scroll).is_err() {
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

                // Scrollback keys (Shift+PageUp/PageDown/Home/End) drive our own
                // viewport, since the host terminal's native scrollback is off
                // on the alternate screen. Everything else is forwarded to the
                // session (and snaps the view back to the live bottom).
                if let Some(action) = parse_scroll_key(&chunk) {
                    let max_scroll = vt.scrollback_rows();
                    let page = usize::from(rows).saturating_sub(1).max(1);
                    let new_scroll = match action {
                        ScrollKey::Up => (scroll + page).min(max_scroll),
                        ScrollKey::Down => scroll.saturating_sub(page),
                        ScrollKey::Top => max_scroll,
                        ScrollKey::Bottom => 0,
                    };
                    if new_scroll != scroll {
                        scroll = new_scroll;
                        if render_now(&mut vt, scroll).is_err() {
                            break Exit::DaemonGone;
                        }
                    }
                    continue;
                }

                if scroll != 0 {
                    scroll = 0;
                    let _ = render_now(&mut vt, scroll);
                }
                if writer.write_frame(&Frame::Input { bytes: chunk }).await.is_err() {
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
                let _ = render_now(&mut vt, scroll);
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
fn render_now(vt: &mut GhosttyVt, scroll: usize) -> std::io::Result<()> {
    vt.set_scroll(scroll);
    let snap = vt.render_snapshot();
    write_stdout(&render::render_frame(&snap))
}

/// A scrollback navigation key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollKey {
    Up,
    Down,
    Top,
    Bottom,
}

/// Recognize the Shift-modified paging keys that drive scrollback. Plain
/// PageUp/Home/etc. are left for the session (returns `None`).
fn parse_scroll_key(chunk: &[u8]) -> Option<ScrollKey> {
    match chunk {
        // Shift+PageUp / Shift+PageDown: CSI 5 ; 2 ~ / CSI 6 ; 2 ~
        b"\x1b[5;2~" => Some(ScrollKey::Up),
        b"\x1b[6;2~" => Some(ScrollKey::Down),
        // Shift+Home / Shift+End: CSI 1 ; 2 H / CSI 1 ; 2 F
        b"\x1b[1;2H" => Some(ScrollKey::Top),
        b"\x1b[1;2F" => Some(ScrollKey::Bottom),
        _ => None,
    }
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

/// Alternate-screen guard. Enters the alternate screen (DEC 1049) so detach
/// restores the caller's screen; on drop, leaves it and resets the cursor
/// shape. Crucially it does **not** enable any mouse-tracking mode, so the host
/// terminal's own drag-to-select and copy keep working.
struct ScreenGuard;

impl ScreenGuard {
    fn enter() -> std::io::Result<Self> {
        write_stdout(b"\x1b[?1049h")?;
        Ok(Self)
    }
}

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        let _ = write_stdout(b"\x1b[?1049l\x1b[0 q");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_paging_keys_drive_scrollback() {
        assert_eq!(parse_scroll_key(b"\x1b[5;2~"), Some(ScrollKey::Up));
        assert_eq!(parse_scroll_key(b"\x1b[6;2~"), Some(ScrollKey::Down));
        assert_eq!(parse_scroll_key(b"\x1b[1;2H"), Some(ScrollKey::Top));
        assert_eq!(parse_scroll_key(b"\x1b[1;2F"), Some(ScrollKey::Bottom));
    }

    #[test]
    fn plain_keys_are_left_for_the_session() {
        // Plain PageUp/Home and ordinary typing are not scrollback keys.
        assert_eq!(parse_scroll_key(b"\x1b[5~"), None);
        assert_eq!(parse_scroll_key(b"\x1b[H"), None);
        assert_eq!(parse_scroll_key(b"ls -la\r"), None);
        assert_eq!(parse_scroll_key(b""), None);
    }
}
