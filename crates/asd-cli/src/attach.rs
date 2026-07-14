//! Interactive attach and the `--stdio` proxy.

use std::io::Write as _;
use std::os::fd::AsFd;

use anyhow::Context;
use asd_proto::{Frame, code};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::client::Client;

/// Interactive detach key: Ctrl-\ (byte 0x1C in raw mode).
const DETACH_BYTE: u8 = 0x1c;

enum Exit {
    Detached,
    SessionEnded(String),
    DaemonGone,
}

/// Interactive attach: raw termios, stdin → Input, Snapshot/Output → stdout,
/// SIGWINCH → Resize, Ctrl-\ for explicit Detach.
pub async fn run(mut client: Client, name: &str) -> anyhow::Result<()> {
    let (cols, rows) = term_size();
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
    eprintln!("[asd: attached to '{name}', detach: Ctrl-\\]");
    let _raw = RawGuard::enable().context("enabling raw terminal mode")?;
    // Session runs on the alternate screen; detach restores the caller's
    // original screen contents (tmux-like). Declared after RawGuard so it
    // drops first: leave the alt screen, then restore termios.
    let _alt = AltScreenGuard::enter().context("entering alternate screen")?;
    paint_snapshot(&snapshot)?;

    // Socket read end as its own task (read_frame is not cancel-safe, so it
    // must not go into a select)
    let (exit_tx, mut exit_rx) = mpsc::channel::<Exit>(1);
    let mut reader = client.reader;
    let reader_task = {
        let exit_tx = exit_tx.clone();
        tokio::spawn(async move {
            loop {
                match reader.read_frame().await {
                    Ok(Some(Frame::Output { bytes })) => {
                        if write_stdout(&bytes).is_err() {
                            let _ = exit_tx.send(Exit::DaemonGone).await;
                            break;
                        }
                    }
                    Ok(Some(Frame::Snapshot { vt })) => {
                        if paint_snapshot(&vt).is_err() {
                            let _ = exit_tx.send(Exit::DaemonGone).await;
                            break;
                        }
                    }
                    Ok(Some(Frame::Error { code: c, msg })) => {
                        let reason = if c == code::SESSION_EXITED {
                            Exit::SessionEnded(msg)
                        } else {
                            Exit::SessionEnded(format!("error {c}: {msg}"))
                        };
                        let _ = exit_tx.send(reason).await;
                        break;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => {
                        let _ = exit_tx.send(Exit::DaemonGone).await;
                        break;
                    }
                }
            }
        })
    };

    // Stdin read end as its own task: convert to Input frames, scan for the
    // detach key
    let (frame_tx, mut frame_rx) = mpsc::channel::<Frame>(64);
    let stdin_task = {
        let exit_tx = exit_tx.clone();
        tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf).await {
                    Ok(0) | Err(_) => {
                        let _ = exit_tx.send(Exit::Detached).await;
                        break;
                    }
                    Ok(n) => {
                        let chunk = &buf[..n];
                        if let Some(pos) = chunk.iter().position(|&b| b == DETACH_BYTE) {
                            if pos > 0 {
                                let _ = frame_tx
                                    .send(Frame::Input {
                                        bytes: chunk[..pos].to_vec(),
                                    })
                                    .await;
                            }
                            let _ = frame_tx.send(Frame::Detach).await;
                            let _ = exit_tx.send(Exit::Detached).await;
                            break;
                        }
                        if frame_tx
                            .send(Frame::Input {
                                bytes: chunk.to_vec(),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        })
    };

    let mut sigwinch = signal(SignalKind::window_change())?;
    let mut writer = client.writer;
    let exit = loop {
        tokio::select! {
            Some(frame) = frame_rx.recv() => {
                if writer.write_frame(&frame).await.is_err() {
                    break Exit::DaemonGone;
                }
            }
            _ = sigwinch.recv() => {
                let (cols, rows) = term_size();
                if writer.write_frame(&Frame::Resize { cols, rows }).await.is_err() {
                    break Exit::DaemonGone;
                }
            }
            reason = exit_rx.recv() => {
                break reason.unwrap_or(Exit::DaemonGone);
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

/// Paint a VT snapshot from scratch: clear and home first — the dump has no
/// leading position sequence and paints relative to the current cursor.
fn paint_snapshot(vt: &[u8]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(vt.len() + 7);
    buf.extend_from_slice(b"\x1b[2J\x1b[H");
    buf.extend_from_slice(vt);
    write_stdout(&buf)
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
/// mouse wheel is translated to arrow keys by the terminal, so wheel
/// scrolling works inside pagers/vim/htop. True scrollback of session
/// history is the M1 FetchHistory feature (spec §4).
struct AltScreenGuard;

impl AltScreenGuard {
    fn enter() -> std::io::Result<Self> {
        write_stdout(b"\x1b[?1049h\x1b[?1007h")?;
        Ok(Self)
    }
}

impl Drop for AltScreenGuard {
    fn drop(&mut self) {
        let _ = write_stdout(b"\x1b[?1007l\x1b[?1049l");
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
