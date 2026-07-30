//! Unix half of the CLI's platform surface: UDS transport, signal-based daemon
//! control, termios raw mode, SIGWINCH. Selected by [`super`]; see there for the
//! shared surface.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use asd_proto::paths;

use super::{BoxRead, BoxWrite, not_running};

// ---- Transport --------------------------------------------------------------

/// Connect to the daemon's UDS and split it for the framed codec.
pub(crate) async fn connect_stream(socket: &Path) -> anyhow::Result<(BoxRead, BoxWrite)> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                not_running(socket)
            }
            _ => anyhow::Error::new(e).context(format!("connecting {}", socket.display())),
        })?;
    let (r, w) = stream.into_split();
    Ok((Box::new(r), Box::new(w)))
}

/// The `--stdio` byte proxy: no handshake and no local VT — the protocol is
/// spoken by the pipe's far end (a remote GUI/CLI) and this process is a pure
/// passthrough. The SSH dumb-pipe scenario: `ssh host "asd attach --stdio"`.
pub(crate) async fn run_stdio_proxy(socket: &Path) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt as _;

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

// ---- Daemon process control -------------------------------------------------

/// Detach a spawned daemon: its own session, so it survives this process and
/// owns no controlling terminal.
pub(crate) fn configure_detached(cmd: &mut std::process::Command) {
    // SAFETY: setsid(2) in the child between fork and exec; async-signal-safe.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(cmd, || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Stop the daemon owning `socket` if one is recorded and alive, for a restart.
/// SIGUSR1 is the daemon's clean-shutdown signal; SIGKILL is the 3s backstop.
pub(crate) async fn stop_daemon(socket: &Path) {
    let pid_path = paths::pid_path(socket);
    if let Some(pid) = std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|&p| p > 0 && process_alive(p))
    {
        // SAFETY: kill(2) with a real signal; failures are ignored (racing exit).
        unsafe { libc::kill(pid, libc::SIGUSR1) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while socket.exists() {
            if Instant::now() >= deadline {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(&pid_path);
}

/// Whether `pid` exists (a `kill(pid, 0)` probe sends no signal).
fn process_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

// ---- Terminal ---------------------------------------------------------------

/// Terminal size; 80×24 when unavailable (not a tty).
pub(crate) fn term_size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if ret == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_col, ws.ws_row)
    } else {
        (80, 24)
    }
}

// ---- Terminal restore on a fatal signal --------------------------------------

/// The restore sequence and the cooked termios the handler needs, published as
/// raw pointers because that is all a signal handler may read. Leaked once at
/// install time; never freed, never rewritten.
static RESTORE: std::sync::atomic::AtomicPtr<u8> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
static RESTORE_LEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static ORIG_TERMIOS: std::sync::atomic::AtomicPtr<libc::termios> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// SIGHUP/SIGTERM/SIGINT handler: put the terminal back, then re-raise the
/// signal with its default disposition so the exit status is unchanged. Only
/// async-signal-safe calls here (write / tcsetattr / signal / raise).
extern "C" fn on_terminating_signal(sig: libc::c_int) {
    use std::sync::atomic::Ordering;
    unsafe {
        let seq = RESTORE.load(Ordering::SeqCst);
        let len = RESTORE_LEN.load(Ordering::SeqCst);
        if !seq.is_null() && len > 0 {
            libc::write(libc::STDOUT_FILENO, seq.cast(), len);
        }
        let orig = ORIG_TERMIOS.load(Ordering::SeqCst);
        if !orig.is_null() {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, orig);
        }
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Arm `restore` to be written to the terminal if this process is killed, and
/// capture the cooked termios so the line discipline goes back too. Call before
/// entering raw mode — being killed skips every `Drop`, which would otherwise
/// leave the terminal in mouse-tracking mode spewing `ESC[<..M` at the shell
/// prompt (`asd ui` arms the same thing in `asd-tui`'s platform layer).
pub(crate) fn install_terminating_signal_restore(restore: Vec<u8>) {
    use std::sync::atomic::Ordering;
    unsafe {
        let leaked = restore.leak();
        RESTORE_LEN.store(leaked.len(), Ordering::SeqCst);
        RESTORE.store(leaked.as_mut_ptr(), Ordering::SeqCst);

        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut t) == 0 {
            ORIG_TERMIOS.store(Box::into_raw(Box::new(t)), Ordering::SeqCst);
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

/// Raw mode guard: restores the original termios on drop.
///
/// The guard owns the mode it has to put back, rather than parking it in a
/// process-wide slot: a second guard would overwrite such a slot with the *raw*
/// termios it just installed, and dropping either one would then restore raw
/// mode as if it were the original. Owning it makes that unrepresentable.
pub(crate) struct RawGuard {
    original: nix::sys::termios::Termios,
}

impl RawGuard {
    pub(crate) fn enable() -> anyhow::Result<Self> {
        use nix::sys::termios::{SetArg, cfmakeraw, tcgetattr, tcsetattr};
        use std::os::fd::AsFd;

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
        use std::os::fd::AsFd;

        let stdin = std::io::stdin();
        let _ = tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &self.original);
    }
}

// ---- Resize notification ----------------------------------------------------

/// SIGWINCH stream.
pub(crate) struct Winch(tokio::signal::unix::Signal);

impl Winch {
    /// Resolves once per resize.
    pub(crate) async fn recv(&mut self) -> Option<()> {
        self.0.recv().await
    }
}

pub(crate) fn winch() -> anyhow::Result<Winch> {
    use tokio::signal::unix::{SignalKind, signal};
    Ok(Winch(signal(SignalKind::window_change())?))
}
