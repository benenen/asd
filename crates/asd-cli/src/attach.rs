//! Interactive attach (with a local scrollback viewer) and the `--stdio` proxy.
//!
//! Painting and mode state live in the main loop; the socket-reader and
//! stdin-reader run as their own tasks feeding it, because `read_frame` and a
//! blocking stdin read are not cancel-safe and must not sit in a `select!`.
//!
//! Two modes:
//! - **Live**: Output/Snapshot are painted; stdin is forwarded as Input.
//!   `PageUp` enters the scrollback viewer, `Ctrl-\` detaches.
//! - **Copy** (scrollback viewer): the client renders a window of the
//!   session's history fetched via `FetchHistory`; live Output is ignored
//!   until the user leaves. Wheel / PgUp/PgDn / arrows / g / G navigate;
//!   `q` / `End` / `Ctrl-\` leave (a `Refresh` resyncs the live screen).

use std::io::Write as _;
use std::os::fd::AsFd;

use anyhow::Context;
use asd_proto::{Frame, code};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::client::Client;

/// Detach key: Ctrl-\ (byte 0x1C in raw mode).
const DETACH_BYTE: u8 = 0x1c;
/// Ctrl-C inside the scrollback viewer leaves it (does not reach the session).
const CTRL_C: u8 = 0x03;

/// Frames the socket-reader task forwards to the main loop.
enum Ev {
    Output(Vec<u8>),
    Snapshot(Vec<u8>),
    History {
        total_rows: u32,
        start: u32,
        rows: Vec<Vec<u8>>,
    },
    /// Session ended or errored; carries the reason to print on exit.
    Ended(Exit),
}

enum Exit {
    Detached,
    SessionEnded(String),
    DaemonGone,
}

/// Scrollback-viewer state. `top` is the screen-space index of the topmost
/// visible line; the window is `[top, top + view_rows)`.
struct Copy {
    top: u32,
    total: u32,
    view_rows: u16,
    /// First reply is a probe to learn `total`; we jump and refetch, so the
    /// probe itself is not painted.
    initialized: bool,
    /// After leaving, we sent `Refresh` and wait for the `Snapshot` to resync;
    /// meanwhile Output/History/input are ignored.
    resyncing: bool,
}

enum Mode {
    Live,
    Copy(Copy),
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

    // The first frame must be Snapshot (or an error); handling errors before
    // entering raw mode is friendlier
    let snapshot = match client.reader.read_frame().await? {
        Some(Frame::Snapshot { vt }) => vt,
        Some(Frame::Error { code, msg }) => anyhow::bail!("attach failed ({code}): {msg}"),
        other => anyhow::bail!("expected Snapshot after Attach, got {other:?}"),
    };

    // Hint lands on the primary screen, so it is still visible after detach
    eprintln!("[asd: attached to '{name}', scrollback: PgUp, detach: Ctrl-\\]");
    let _raw = RawGuard::enable().context("enabling raw terminal mode")?;
    // Session runs on the alternate screen; detach restores the caller's
    // original screen contents (tmux-like). Declared after RawGuard so it
    // drops first: leave the alt screen, then restore termios.
    let _alt = AltScreenGuard::enter().context("entering alternate screen")?;
    paint_snapshot(&snapshot)?;

    // Socket reader → Ev channel (no direct painting).
    let (ev_tx, mut ev_rx) = mpsc::channel::<Ev>(256);
    let mut reader = client.reader;
    let reader_task = tokio::spawn(async move {
        loop {
            let ev = match reader.read_frame().await {
                Ok(Some(Frame::Output { bytes })) => Ev::Output(bytes),
                Ok(Some(Frame::Snapshot { vt })) => Ev::Snapshot(vt),
                Ok(Some(Frame::History {
                    total_rows,
                    start,
                    rows,
                })) => Ev::History {
                    total_rows,
                    start,
                    rows,
                },
                Ok(Some(Frame::Error { code: c, msg })) => {
                    let reason = if c == code::SESSION_EXITED {
                        Exit::SessionEnded(msg)
                    } else {
                        Exit::SessionEnded(format!("error {c}: {msg}"))
                    };
                    let _ = ev_tx.send(Ev::Ended(reason)).await;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => {
                    let _ = ev_tx.send(Ev::Ended(Exit::DaemonGone)).await;
                    break;
                }
            };
            if ev_tx.send(ev).await.is_err() {
                break;
            }
        }
    });

    // Stdin reader → raw byte chunks; None on EOF.
    let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(64);
    let stdin_task = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 4096];
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
    let mut mode = Mode::Live;

    let exit = 'main: loop {
        tokio::select! {
            ev = ev_rx.recv() => {
                let Some(ev) = ev else { break Exit::DaemonGone };
                match ev {
                    Ev::Ended(reason) => break reason,
                    Ev::Output(bytes) => {
                        // Ignored while viewing scrollback (repainted on exit).
                        if matches!(mode, Mode::Live) && write_stdout(&bytes).is_err() {
                            break Exit::DaemonGone;
                        }
                    }
                    Ev::Snapshot(vt) => {
                        // Attach reply or the Refresh resync after leaving copy.
                        if paint_snapshot(&vt).is_err() {
                            break Exit::DaemonGone;
                        }
                        mode = Mode::Live;
                    }
                    Ev::History { total_rows, start, rows } => {
                        if let Mode::Copy(c) = &mut mode {
                            if c.resyncing { continue; }
                            c.total = total_rows;
                            if !c.initialized {
                                // Probe reply: learn total, jump up a page, refetch.
                                c.initialized = true;
                                let page = u32::from(c.view_rows);
                                let max_top = total_rows.saturating_sub(u32::from(c.view_rows));
                                c.top = max_top.saturating_sub(page);
                                let req = Frame::FetchHistory { start: c.top, count: u32::from(c.view_rows) };
                                if writer.write_frame(&req).await.is_err() {
                                    break Exit::DaemonGone;
                                }
                            } else {
                                c.top = start;
                                if paint_copy(&rows, start, total_rows, cols, c.view_rows).is_err() {
                                    break Exit::DaemonGone;
                                }
                            }
                        }
                    }
                }
            }
            chunk = in_rx.recv() => {
                let Some(chunk) = chunk else { break Exit::Detached };
                match &mut mode {
                    Mode::Live => {
                        match handle_live_input(&chunk) {
                            LiveAction::Detach { forward } => {
                                if !forward.is_empty() {
                                    let _ = writer.write_frame(&Frame::Input { bytes: forward }).await;
                                }
                                let _ = writer.write_frame(&Frame::Detach).await;
                                break Exit::Detached;
                            }
                            LiveAction::EnterCopy { forward } => {
                                if !forward.is_empty() {
                                    let _ = writer.write_frame(&Frame::Input { bytes: forward }).await;
                                }
                                let view_rows = rows.saturating_sub(1).max(1);
                                enter_copy(&mut mode, view_rows);
                                // Probe fetch to learn total; reply drives the jump.
                                let req = Frame::FetchHistory { start: 0, count: u32::from(view_rows) };
                                if writer.write_frame(&req).await.is_err() {
                                    break Exit::DaemonGone;
                                }
                            }
                            LiveAction::Forward(bytes) => {
                                if writer.write_frame(&Frame::Input { bytes }).await.is_err() {
                                    break Exit::DaemonGone;
                                }
                            }
                        }
                    }
                    Mode::Copy(c) => {
                        if c.resyncing { continue; }
                        match handle_copy_input(&chunk) {
                            CopyAction::Detach => {
                                let _ = writer.write_frame(&Frame::Detach).await;
                                break Exit::Detached;
                            }
                            CopyAction::Leave => {
                                c.resyncing = true;
                                disable_mouse();
                                if writer.write_frame(&Frame::Refresh).await.is_err() {
                                    break Exit::DaemonGone;
                                }
                            }
                            CopyAction::Scroll(delta) => {
                                if !c.initialized { continue; }
                                let max_top = c.total.saturating_sub(u32::from(c.view_rows));
                                let new_top = apply_delta(c.top, delta, max_top);
                                if new_top != c.top || c.total == 0 {
                                    c.top = new_top;
                                    let req = Frame::FetchHistory { start: new_top, count: u32::from(c.view_rows) };
                                    if writer.write_frame(&req).await.is_err() {
                                        break Exit::DaemonGone;
                                    }
                                }
                            }
                            CopyAction::None => {}
                        }
                    }
                }
            }
            _ = sigwinch.recv() => {
                let (c, r) = term_size();
                cols = c; rows = r;
                if writer.write_frame(&Frame::Resize { cols, rows }).await.is_err() {
                    break Exit::DaemonGone;
                }
                if let Mode::Copy(c) = &mut mode {
                    c.view_rows = rows.saturating_sub(1).max(1);
                    if c.initialized && !c.resyncing {
                        let max_top = c.total.saturating_sub(u32::from(c.view_rows));
                        c.top = c.top.min(max_top);
                        let req = Frame::FetchHistory { start: c.top, count: u32::from(c.view_rows) };
                        if writer.write_frame(&req).await.is_err() {
                            break 'main Exit::DaemonGone;
                        }
                    }
                }
            }
        }
    };

    reader_task.abort();
    stdin_task.abort();
    drop(_alt);
    drop(_raw);

    // Printed on the restored primary screen, right under the attach hint
    match exit {
        Exit::Detached => eprintln!("[asd: detached]"),
        Exit::SessionEnded(msg) => eprintln!("[asd: {msg}]"),
        Exit::DaemonGone => eprintln!("[asd: connection to daemon lost]"),
    }
    Ok(())
}

fn enter_copy(mode: &mut Mode, view_rows: u16) {
    enable_mouse();
    *mode = Mode::Copy(Copy {
        top: 0,
        total: 0,
        view_rows,
        initialized: false,
        resyncing: false,
    });
}

/// How much to move `top` by (positive = toward the bottom / newer lines).
fn apply_delta(top: u32, delta: i64, max_top: u32) -> u32 {
    let next = i64::from(top) + delta;
    next.clamp(0, i64::from(max_top)) as u32
}

// ---- input classification ----

enum LiveAction {
    /// Detach; `forward` is any input preceding the detach key in the chunk.
    Detach {
        forward: Vec<u8>,
    },
    /// Enter the scrollback viewer; `forward` is input preceding PageUp.
    EnterCopy {
        forward: Vec<u8>,
    },
    Forward(Vec<u8>),
}

/// PageUp: `ESC [ 5 ~`.
const PAGE_UP: &[u8] = b"\x1b[5~";

fn handle_live_input(chunk: &[u8]) -> LiveAction {
    if let Some(pos) = chunk.iter().position(|&b| b == DETACH_BYTE) {
        return LiveAction::Detach {
            forward: chunk[..pos].to_vec(),
        };
    }
    if let Some(pos) = find_subseq(chunk, PAGE_UP) {
        return LiveAction::EnterCopy {
            forward: chunk[..pos].to_vec(),
        };
    }
    LiveAction::Forward(chunk.to_vec())
}

enum CopyAction {
    /// Leave the viewer and resync the live screen.
    Leave,
    /// Detach entirely.
    Detach,
    /// Move `top` by this many lines (negative = up/older).
    Scroll(i64),
    None,
}

fn handle_copy_input(chunk: &[u8]) -> CopyAction {
    // Recognize one salient action per chunk; key presses arrive whole.
    if chunk.contains(&DETACH_BYTE) {
        return CopyAction::Detach;
    }
    if chunk.contains(&CTRL_C) || chunk.contains(&b'q') {
        return CopyAction::Leave;
    }
    if chunk.contains(&b'g') {
        return CopyAction::Scroll(i64::MIN); // to top
    }
    if chunk.contains(&b'G') {
        return CopyAction::Scroll(i64::MAX); // to bottom
    }
    // SGR mouse wheel: ESC [ < 64/65 ; ... M
    if find_subseq(chunk, b"\x1b[<64").is_some() {
        return CopyAction::Scroll(-3);
    }
    if find_subseq(chunk, b"\x1b[<65").is_some() {
        return CopyAction::Scroll(3);
    }
    if find_subseq(chunk, PAGE_UP).is_some() {
        return CopyAction::Scroll(-16);
    }
    if find_subseq(chunk, b"\x1b[6~").is_some() {
        // PageDown
        return CopyAction::Scroll(16);
    }
    if find_subseq(chunk, b"\x1b[A").is_some() {
        return CopyAction::Scroll(-1);
    }
    if find_subseq(chunk, b"\x1b[B").is_some() {
        return CopyAction::Scroll(1);
    }
    if find_subseq(chunk, b"\x1b[H").is_some() || find_subseq(chunk, b"\x1b[1~").is_some() {
        return CopyAction::Scroll(i64::MIN);
    }
    if find_subseq(chunk, b"\x1b[F").is_some() || find_subseq(chunk, b"\x1b[4~").is_some() {
        return CopyAction::Scroll(i64::MAX);
    }
    CopyAction::None
}

fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---- painting ----

/// Paint a VT snapshot from scratch: clear and home first — the dump has no
/// leading position sequence and paints relative to the current cursor.
fn paint_snapshot(vt: &[u8]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(vt.len() + 7);
    buf.extend_from_slice(b"\x1b[2J\x1b[H");
    buf.extend_from_slice(vt);
    write_stdout(&buf)
}

/// Paint the scrollback window plus a reverse-video status bar on the last row.
fn paint_copy(
    rows: &[Vec<u8>],
    top: u32,
    total: u32,
    cols: u16,
    view_rows: u16,
) -> std::io::Result<()> {
    let cols = usize::from(cols).max(1);
    let mut buf = Vec::new();
    buf.extend_from_slice(b"\x1b[2J\x1b[H");
    for (i, line) in rows.iter().take(usize::from(view_rows)).enumerate() {
        if i > 0 {
            buf.extend_from_slice(b"\r\n");
        }
        // Lines are already <= cols wide; clear to EOL to erase stale glyphs.
        buf.extend_from_slice(line);
        buf.extend_from_slice(b"\x1b[K");
    }

    // Status bar: move to the last row, reverse video, padded to full width.
    let last_row = view_rows + 1;
    let bottom = (top as usize + usize::from(view_rows)).min(total as usize);
    let mut status = format!(
        " SCROLLBACK {}-{}/{}   PgUp/PgDn ↑↓ wheel  g/G top/bottom  q live ",
        top as usize + 1,
        bottom,
        total,
    );
    truncate_to_cols(&mut status, cols);
    while display_len(&status) < cols {
        status.push(' ');
    }
    buf.extend_from_slice(format!("\x1b[{last_row};1H").as_bytes());
    buf.extend_from_slice(b"\x1b[7m");
    buf.extend_from_slice(status.as_bytes());
    buf.extend_from_slice(b"\x1b[0m");
    write_stdout(&buf)
}

/// Rough display width (counts a CJK/wide char as 2 columns). Good enough for
/// clamping the status bar.
fn display_len(s: &str) -> usize {
    s.chars().map(char_cols).sum()
}

fn char_cols(c: char) -> usize {
    // Treat common CJK/full-width ranges as width 2.
    let u = c as u32;
    let wide = matches!(u,
        0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 |
        0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 |
        0x20000..=0x3FFFD);
    if wide { 2 } else { 1 }
}

fn truncate_to_cols(s: &mut String, cols: usize) {
    if display_len(s) <= cols {
        return;
    }
    let mut acc = 0;
    let mut end = s.len();
    for (i, c) in s.char_indices() {
        if acc + char_cols(c) > cols {
            end = i;
            break;
        }
        acc += char_cols(c);
    }
    s.truncate(end);
}

// ---- mouse reporting (only enabled inside the scrollback viewer) ----

fn enable_mouse() {
    let _ = write_stdout(b"\x1b[?1000h\x1b[?1006h");
}

fn disable_mouse() {
    let _ = write_stdout(b"\x1b[?1000l\x1b[?1006l");
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

/// Alternate-screen guard: enters the alternate screen (DEC 1049, clears it
/// and saves the cursor) on creation, and restores the primary screen with
/// its previous contents on drop.
///
/// Also enables "alternate scroll" (DEC 1007): on the alternate screen the
/// mouse wheel is translated to arrow keys by the terminal, so wheel scrolling
/// works inside pagers/vim/htop. Scrolling the session's own history is the
/// scrollback viewer (`PageUp`), which uses `FetchHistory` (spec §4).
struct AltScreenGuard;

impl AltScreenGuard {
    fn enter() -> std::io::Result<Self> {
        write_stdout(b"\x1b[?1049h\x1b[?1007h")?;
        Ok(Self)
    }
}

impl Drop for AltScreenGuard {
    fn drop(&mut self) {
        // Also disable mouse reporting in case we drop mid scrollback view.
        let _ = write_stdout(b"\x1b[?1000l\x1b[?1006l\x1b[?1007l\x1b[?1049l");
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
