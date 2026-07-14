//! Interactive attach as a VT-render client, plus the `--stdio` proxy.
//!
//! Unlike a dumb passthrough, this client keeps its own [`GhosttyVt`] terminal
//! (fed the daemon's Snapshot + Output) and renders it to the screen itself.
//! That local VT model gives a single Live view (modeled on boo's `boo ui`):
//! - **detach restores the original screen** (we run on the alternate screen);
//! - **wheel scrolls** back through history — a client-local action that never
//!   disturbs other attached clients (keyboard Shift+PageUp/PageDown/Home/End
//!   work too);
//! - **drag selects + copies** — we grab the mouse (1002+1006, so drag motion
//!   is reported), highlight the selection, and write the system clipboard via
//!   OSC 52. Hold **Shift** while dragging to fall back to the host terminal's
//!   own native selection instead.
//!
//! When the session program itself wants the mouse (vim/htop), we mirror its
//! exact modes to the host and forward the events to it (`sync_host_mouse`).

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
/// Lines the wheel scrolls per tick while in the scrollback view.
const WHEEL_STEP: usize = 3;
/// DEC private mouse modes we mirror/disable (ascending — matches
/// `VtBackend::mouse_modes`).
const MOUSE_MODES: &[u16] = &[9, 1000, 1002, 1003, 1005, 1006, 1015, 1016];

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
    // Alt screen only — no mouse tracking is enabled here. We enable/disable it
    // dynamically to mirror the session (see `sync_host_mouse`). Dropped before
    // RawGuard.
    let _screen = ScreenGuard::enter().context("entering alternate screen")?;

    // Our local mirror of the session terminal.
    let mut vt = GhosttyVt::new(cols.max(1), rows.max(1), 100_000);
    vt.feed(&first);
    let _ = vt.take_pty_responses(); // the daemon already answered any queries

    // Lines scrolled up from the live bottom (0 = following live output).
    let mut scroll = 0usize;
    // Mouse modes currently enabled on the host terminal. When the session
    // wants the mouse (vim) we mirror its exact modes; otherwise we keep our
    // own base (1002+1006) so the wheel scrolls and drags select locally.
    let mut host_mouse: Vec<u16> = Vec::new();
    // Whether the session program currently wants the mouse (routes events:
    // true → forward to it; false → wheel scrolls / drag selects locally).
    let mut session_mouse = false;
    // Active drag selection (viewport cells), while `selecting`.
    let mut selection: Option<render::Selection> = None;
    let mut selecting = false;

    render_now(&mut vt, scroll, selection)?;
    sync_host_mouse(&mut vt, &mut host_mouse, &mut session_mouse)?;

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
                        // The session may have toggled its mouse mode; mirror
                        // it (or fall back to our base) onto the host terminal.
                        if sync_host_mouse(&mut vt, &mut host_mouse, &mut session_mouse).is_err() {
                            break Exit::DaemonGone;
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
                        if sync_host_mouse(&mut vt, &mut host_mouse, &mut session_mouse).is_err() {
                            break Exit::DaemonGone;
                        }
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

                // Scrollback keys (Shift+PageUp/PageDown/Home/End) always drive
                // our own viewport (the host's native scrollback is off on the
                // alternate screen).
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
                        if render_now(&mut vt, scroll, selection).is_err() {
                            break Exit::DaemonGone;
                        }
                    }
                    continue;
                }

                if is_mouse_report(&chunk) {
                    // When the session wants the mouse (vim/htop), the event is
                    // its business: forward verbatim in the live view (the host
                    // mirrors the session's encoding, and the viewport is 1:1,
                    // so no translation is needed). While scrolled back, the
                    // wheel still scrolls our view.
                    if session_mouse && scroll == 0 {
                        if writer.write_frame(&Frame::Input { bytes: chunk }).await.is_err() {
                            break Exit::DaemonGone;
                        }
                        continue;
                    }
                    // Otherwise the mouse is ours (shell prompt, or scrolled
                    // back): wheel scrolls, drag selects + copies via OSC 52.
                    let max_scroll = vt.scrollback_rows();
                    let mut dirty = false;
                    for ev in render::parse_mouse(&chunk) {
                        match ev.kind {
                            render::MouseKind::WheelUp => {
                                scroll = (scroll + WHEEL_STEP).min(max_scroll);
                                dirty = true;
                            }
                            render::MouseKind::WheelDown => {
                                scroll = scroll.saturating_sub(WHEEL_STEP);
                                dirty = true;
                            }
                            render::MouseKind::Press => {
                                selecting = true;
                                selection = Some(render::Selection {
                                    start: (ev.x, ev.y),
                                    end: (ev.x, ev.y),
                                });
                                dirty = true;
                            }
                            render::MouseKind::Drag if selecting => {
                                if let Some(sel) = &mut selection {
                                    sel.end = (ev.x, ev.y);
                                    dirty = true;
                                }
                            }
                            render::MouseKind::Release if selecting => {
                                selecting = false;
                                if let Some(sel) = selection {
                                    // Position the viewport at the current
                                    // scroll so selection coords match what is
                                    // displayed, then copy via OSC 52.
                                    vt.set_scroll(scroll);
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
                            _ => {}
                        }
                    }
                    if dirty && render_now(&mut vt, scroll, selection).is_err() {
                        break Exit::DaemonGone;
                    }
                    continue;
                }

                // Typing: snap back to the live bottom, clear any selection, and
                // forward.
                if scroll != 0 || selection.is_some() {
                    scroll = 0;
                    selection = None;
                    let _ = render_now(&mut vt, scroll, selection);
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
                let _ = render_now(&mut vt, scroll, selection);
            }
        }
    };

    reader_task.abort();
    stdin_task.abort();
    drop(_screen);
    drop(_raw);

    // A normal detach returns silently to the shell prompt (the alt-screen
    // restore already put the cursor back). Only abnormal exits print a reason.
    match exit {
        Exit::Detached => {}
        Exit::SessionEnded(msg) => eprintln!("[asd: {msg}]"),
        Exit::DaemonGone => eprintln!("[asd: connection to daemon lost]"),
    }
    Ok(())
}

/// Position the viewport at `scroll` and paint one frame (with the active
/// selection highlighted, if any).
fn render_now(
    vt: &mut GhosttyVt,
    scroll: usize,
    sel: Option<render::Selection>,
) -> std::io::Result<()> {
    vt.set_scroll(scroll);
    let snap = vt.render_snapshot();
    write_stdout(&render::render_frame(&snap, sel))
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

/// Our base mouse modes when the session does not want the mouse: button-event
/// tracking (1002, so drags are reported for local text selection) + SGR
/// encoding (1006). Matches boo's `boo ui`.
const BASE_MOUSE: &[u16] = &[1002, 1006];

/// Keep the host terminal's mouse modes in sync. When the session wants the
/// mouse (vim/htop) we mirror its exact modes so its events arrive in the
/// encoding it expects; otherwise we assert our own base (1002+1006) so the
/// wheel scrolls and drags select locally. `host` and `session_mouse` are
/// updated in place; only the delta is emitted.
fn sync_host_mouse(
    vt: &mut GhosttyVt,
    host: &mut Vec<u16>,
    session_mouse: &mut bool,
) -> std::io::Result<()> {
    *session_mouse = vt.is_mouse_tracking();
    let want = if *session_mouse {
        vt.mouse_modes()
    } else {
        BASE_MOUSE.to_vec()
    };
    if want == *host {
        return Ok(());
    }
    let seq = mouse_mode_delta(host, &want);
    if !seq.is_empty() {
        write_stdout(&seq)?;
    }
    *host = want;
    Ok(())
}

/// The DEC private-mode toggles to move the host from `old` to `new`:
/// `CSI ? n l` for modes being dropped, `CSI ? n h` for modes being added.
fn mouse_mode_delta(old: &[u16], new: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    for m in old {
        if !new.contains(m) {
            out.extend_from_slice(format!("\x1b[?{m}l").as_bytes());
        }
    }
    for m in new {
        if !old.contains(m) {
            out.extend_from_slice(format!("\x1b[?{m}h").as_bytes());
        }
    }
    out
}

/// Whether a chunk is a mouse report: SGR (`CSI < ...`) or legacy X10/UTF-8
/// (`CSI M ...`). These only arrive when host mouse tracking is on.
fn is_mouse_report(chunk: &[u8]) -> bool {
    chunk.starts_with(b"\x1b[<") || chunk.starts_with(b"\x1b[M")
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
/// restores the caller's screen; on drop, disables every mouse mode we may
/// have enabled, leaves the alt screen, and resets the cursor shape. It does
/// not enable mouse tracking itself — `sync_host_mouse` does that right after
/// the first paint (our base 1002+1006, or the session's exact modes).
struct ScreenGuard;

impl ScreenGuard {
    fn enter() -> std::io::Result<Self> {
        write_stdout(b"\x1b[?1049h")?;
        Ok(Self)
    }
}

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        let mut seq = Vec::new();
        for m in MOUSE_MODES {
            seq.extend_from_slice(format!("\x1b[?{m}l").as_bytes());
        }
        seq.extend_from_slice(b"\x1b[?1049l\x1b[0 q");
        let _ = write_stdout(&seq);
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

    #[test]
    fn mode_delta_emits_only_changes() {
        // Off → normal+SGR: enable both, in ascending order.
        assert_eq!(
            mouse_mode_delta(&[], &[1000, 1006]),
            b"\x1b[?1000h\x1b[?1006h"
        );
        // Add button tracking (1002): only the new one is enabled.
        assert_eq!(
            mouse_mode_delta(&[1000, 1006], &[1000, 1002, 1006]),
            b"\x1b[?1002h"
        );
        // Session turns mouse off: disable everything that was on.
        assert_eq!(
            mouse_mode_delta(&[1000, 1002, 1006], &[]),
            b"\x1b[?1000l\x1b[?1002l\x1b[?1006l"
        );
        // No change: nothing emitted.
        assert!(mouse_mode_delta(&[1000, 1006], &[1000, 1006]).is_empty());
    }

    #[test]
    fn detects_mouse_reports() {
        assert!(is_mouse_report(b"\x1b[<0;10;5M")); // SGR press
        assert!(is_mouse_report(b"\x1b[<64;1;1M")); // SGR wheel
        assert!(is_mouse_report(b"\x1b[M \"5")); // legacy
        assert!(!is_mouse_report(b"ls\r"));
        assert!(!is_mouse_report(b"\x1b[A")); // arrow key
    }
}
