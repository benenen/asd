//! Unix half: the UDS transport plus terminal hygiene (signal-driven restore
//! and a hangup watchdog). Selected by [`super`]; see there for the shared
//! surface.

use std::path::Path;
use std::time::Duration;

use super::{BoxRead, BoxWrite, TERM_RESTORE};

/// Connect to the daemon's UDS and split it for the framed codec.
pub(crate) async fn connect_stream(socket: &Path) -> Result<(BoxRead, BoxWrite), String> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let (r, w) = stream.into_split();
    Ok((Box::new(r), Box::new(w)))
}

/// The cooked termios captured before raw mode, so the signal handler can put
/// the line discipline back. A leaked box, loaded via an async-signal-safe
/// atomic read inside the handler.
static ORIG_TERMIOS: std::sync::atomic::AtomicPtr<libc::termios> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// SIGHUP/SIGTERM/SIGINT handler: a kill or a closed terminal (SSH drop) skips
/// `run`'s normal cleanup, which would leave the terminal in mouse-tracking mode
/// spewing `ESC[<..M` on every mouse move. Restore the terminal, then re-raise
/// the signal with the default disposition so the exit status is unchanged. Only
/// async-signal-safe calls here (write / tcsetattr / signal / raise).
extern "C" fn on_terminating_signal(sig: libc::c_int) {
    unsafe {
        libc::write(
            libc::STDOUT_FILENO,
            TERM_RESTORE.as_ptr().cast(),
            TERM_RESTORE.len(),
        );
        let orig = ORIG_TERMIOS.load(std::sync::atomic::Ordering::SeqCst);
        if !orig.is_null() {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, orig);
        }
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Capture the cooked termios and install the terminal-restore handlers for the
/// signals that would otherwise kill the process without cleanup. Call before
/// entering raw mode.
pub(crate) fn install_terminating_signal_restore() {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut t) == 0 {
            ORIG_TERMIOS.store(
                Box::into_raw(Box::new(t)),
                std::sync::atomic::Ordering::SeqCst,
            );
        }
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_terminating_signal as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        for sig in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT] {
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// Exit when the hosting terminal disappears out from under us. When the pty
/// backing stdin is destroyed without a SIGHUP reaching this process (orphaned:
/// the terminal emulator died, or the parent shell was SIGKILLed), reads on the
/// dead fd return EOF forever — and crossterm 0.29's event source spins on
/// them without ever returning (its read loop treats `Ok(0)` as neither data
/// nor an error), so the event loop never regains control and the TUI turns
/// into an invisible 100%-CPU process. A `poll` with no requested events still
/// reports `POLLHUP`/`POLLERR`, so this thread sleeps at zero cost (it never
/// wakes for keyboard input) until the terminal is gone, then exits: the
/// daemon treats the dropped connection as a detach, and there is no terminal
/// left to restore.
pub(crate) fn spawn_tty_watchdog() {
    std::thread::Builder::new()
        .name("asd-tui-ttywatch".into())
        .spawn(|| {
            loop {
                let mut pfd = libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: 0,
                    revents: 0,
                };
                let n = unsafe { libc::poll(&mut pfd, 1, -1) };
                if n > 0 && pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                    std::process::exit(0);
                }
                // EINTR or a spurious wake: back off briefly and re-arm.
                std::thread::sleep(Duration::from_millis(200));
            }
        })
        .expect("tty watchdog thread");
}
