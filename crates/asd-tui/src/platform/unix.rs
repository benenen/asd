//! Unix half: the UDS transport plus terminal hygiene (signal-driven restore
//! and a hangup watchdog). Selected by [`super`]; see there for the shared
//! surface.

use std::path::Path;
use std::time::Duration;

use super::{BoxRead, BoxWrite};

/// Connect to the daemon's UDS and split it for the framed codec.
pub(crate) async fn connect_stream(socket: &Path) -> Result<(BoxRead, BoxWrite), String> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let (r, w) = stream.into_split();
    Ok((Box::new(r), Box::new(w)))
}

/// Escape sequences that undo the modes the TUI turns on: close synchronized
/// output first (2026), disable mouse tracking (SGR 1006/1015 + 1000/1002/1003),
/// bracketed paste (2004), leave the alternate screen (1049), show the cursor
/// (25h), reset SGR (0m). Written verbatim from the signal handler, which is the
/// only thing that needs them — hence here and not in the shared module.
const TERM_RESTORE: &[u8] =
    b"\x1b[?2026l\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?1049l\x1b[?25h\x1b[0m";

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

/// The cwd of a live local process, or `None` when it cannot be read.
///
/// This is the same `/proc` read the daemon does in
/// `asd-daemon/src/platform/unix.rs`. It is duplicated rather than shared
/// because `asd-tui` may not depend on `asd-daemon`: the TUI is a terminal-side
/// client and carries no PTY or process management.
///
/// macOS has no `/proc`, so the read simply fails and the caller reports that
/// the session's directory could not be determined — no three-way OS split.
pub(crate) fn session_cwd(pid: u32) -> Option<std::path::PathBuf> {
    if pid == 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_process_cwd() {
        let mine = session_cwd(std::process::id()).expect("/proc/self/cwd is readable");
        assert_eq!(
            mine.canonicalize().unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
    }

    /// The overlay's wrapper around `session_cwd`, exercised here rather than
    /// beside the rest of `graph_overlay`'s tests: it only holds where
    /// `session_cwd` can read a directory at all, and a `#[cfg(unix)]` at a
    /// call site is exactly what this module exists to absorb. Windows'
    /// `session_cwd` is a documented `None`, so there the overlay reports that
    /// the directory could not be determined — for every pid, its own
    /// included.
    #[test]
    fn the_overlay_resolves_a_live_pid_to_its_directory() {
        let path =
            crate::graph_overlay::resolve_repo_path(std::process::id()).expect("own pid resolves");
        assert_eq!(
            path.canonicalize().unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
    }

    #[test]
    fn pid_zero_is_never_a_process() {
        assert!(session_cwd(0).is_none());
    }

    #[test]
    fn an_unknown_pid_is_none_rather_than_a_panic() {
        // A pid this high is not allocated on any default Linux configuration.
        assert!(session_cwd(u32::MAX).is_none());
    }
}
