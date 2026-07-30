//! Windows half of the CLI's platform surface: named-pipe transport, handle
//! based daemon control, console raw mode. Selected by [`super`]; see there for
//! the shared surface.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use asd_proto::paths;

use super::{BoxRead, BoxWrite, not_running};

// ---- Transport --------------------------------------------------------------

/// Connect to the daemon's named pipe and split it for the framed codec.
pub(crate) async fn connect_stream(socket: &Path) -> anyhow::Result<(BoxRead, BoxWrite)> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let pipe_name = socket.to_str().context("pipe path is not valid UTF-8")?;
    // Only "no such pipe" means no daemon. Everything else — every instance
    // busy, access denied, … — must keep its real error, or the user chases a
    // daemon that is in fact running.
    let stream = ClientOptions::new().open(pipe_name).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            not_running(socket)
        } else {
            anyhow::Error::new(e).context(format!("connecting {pipe_name}"))
        }
    })?;
    let (r, w) = tokio::io::split(stream);
    Ok((Box::new(r), Box::new(w)))
}

/// The `--stdio` byte proxy. Unimplemented here: it exists to be the remote end
/// of `ssh host "asd attach --stdio"`, and this binary is not an ssh server.
pub(crate) async fn run_stdio_proxy(_socket: &Path) -> anyhow::Result<()> {
    anyhow::bail!("--stdio proxy is not supported on Windows")
}

// ---- Daemon process control -------------------------------------------------

/// Detach a spawned daemon: no console window of its own.
pub(crate) fn configure_detached(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// Stop the daemon owning `socket` if one is recorded, for a restart.
///
/// Windows has no signals, so the recorded pid is ended with TerminateProcess.
/// That is abrupt — the daemon does not get to refresh each session's live cwd
/// on the way out — but the session list on disk is rewritten on every
/// create/rename/kill, so a restart still restores every session at its last
/// recorded cwd. That is the same guarantee a SIGKILLed unix daemon gives.
pub(crate) async fn stop_daemon(socket: &Path) {
    let pid_path = paths::pid_path(socket);
    if let Some(pid) = std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&p| p > 0)
    {
        win32::terminate(pid);
        // The successor claims the pipe name with `first_pipe_instance(true)`
        // and would fail outright if the old one still held it, so wait for the
        // name to be released before spawning.
        let deadline = Instant::now() + Duration::from_secs(3);
        while pipe_in_use(socket) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let _ = std::fs::remove_file(&pid_path);
}

/// Whether any process still serves the named pipe. Anything other than "no
/// such pipe" — all instances busy, access denied — still means it exists.
fn pipe_in_use(socket: &Path) -> bool {
    use tokio::net::windows::named_pipe::ClientOptions;
    let Some(name) = socket.to_str() else {
        return false;
    };
    match ClientOptions::new().open(name) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Minimal kernel32 process control (no extra crate, mirrors asd-daemon's).
mod win32 {
    unsafe extern "system" {
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> isize;
        fn TerminateProcess(hProcess: isize, uExitCode: u32) -> i32;
        fn CloseHandle(hObject: isize) -> i32;
    }

    const PROCESS_TERMINATE: u32 = 0x0001;

    /// End `pid`. Failure means it is already gone (or not ours to kill), which
    /// for a restart is indistinguishable from success.
    pub fn terminate(pid: u32) {
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        if handle != 0 {
            unsafe {
                TerminateProcess(handle, 0);
                CloseHandle(handle);
            }
        }
    }
}

// ---- Terminal ---------------------------------------------------------------

/// Terminal size; 80×24 when unavailable (not a tty).
pub(crate) fn term_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

/// Arm a terminal restore for a fatal signal — nothing to arm here.
///
/// Windows has no SIGHUP/SIGTERM: a console app is ended with TerminateProcess
/// (no notification at all) or told to stop through a console control handler,
/// which is a different mechanism from the unix half's signal handler. Until one
/// is wired up, `Drop` on the normal exit path is the only restore, so this
/// keeps the surface identical and does nothing.
pub(crate) fn install_terminating_signal_restore(_restore: Vec<u8>) {}

/// Raw mode guard: restores the console mode crossterm saved, on drop.
pub(crate) struct RawGuard;

impl RawGuard {
    pub(crate) fn enable() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode().context("enabling raw terminal mode")?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

// ---- Resize notification ----------------------------------------------------

/// Resize source. Windows delivers resize as a console event rather than a
/// signal, so this never resolves and the select arm is simply never taken; the
/// size is still picked up on the next explicit resize. Mirrors the unix
/// `Winch` shape so the call site stays platform-independent.
pub(crate) struct Winch;

impl Winch {
    pub(crate) async fn recv(&mut self) -> Option<()> {
        std::future::pending().await
    }
}

pub(crate) fn winch() -> anyhow::Result<Winch> {
    Ok(Winch)
}
