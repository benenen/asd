//! Windows-specific daemon operations: Named Pipe listener, shutdown, process
//! control. Selected by [`super`]; see there for the shared surface.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use asd_proto::paths;
use tracing::{error, info, warn};

use crate::conn;
use crate::registry::Registry;
use crate::session::SessionMsg;

// ---- Listener ---------------------------------------------------------------

pub(crate) async fn serve_connections(
    pipe_path: PathBuf,
    registry: Arc<Mutex<Registry>>,
) -> anyhow::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let pipe_name = pipe_path
        .to_str()
        .context("pipe path is not valid UTF-8")?
        .to_string();

    // The FIRST instance claims the name exclusively. Without this flag any
    // other process — including a second asd daemon started while this one is
    // still alive — can add its own instances to the same name, after which
    // clients are routed to whichever instance happens to accept them and the
    // session list silently splits across two daemons. Failing here instead is
    // exactly what makes "a daemon is already running" detectable.
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe_name)
        .with_context(|| {
            format!("creating named pipe {pipe_name} (is another asd daemon already running?)")
        })?;
    info!(pipe = %pipe_name, version = env!("CARGO_PKG_VERSION"), "asd daemon listening");

    // Record our pid so `asd restart` can stop us. Named pipes carry no path in
    // the filesystem, so this goes in the data dir (see `paths::pid_path`).
    let pid_path = paths::pid_path(&pipe_path);
    if let Some(dir) = pid_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&pid_path, std::process::id().to_string()) {
        warn!(error = %e, "failed to write pid file");
    }

    let mut conn_id: u64 = 0;
    loop {
        tokio::select! {
            result = server.connect() => {
                if let Err(e) = result {
                    error!(error = %e, "named pipe connect failed");
                    continue;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C received, shutting down");
                break;
            }
        }

        // Create the successor instance *before* handing the connected one to
        // its task: between a client connecting and the next instance existing
        // there is no instance listening, and a client arriving in that window
        // gets ERROR_PIPE_BUSY.
        conn_id += 1;
        match ServerOptions::new().create(&pipe_name) {
            Ok(next) => {
                let connected = std::mem::replace(&mut server, next);
                spawn_conn(connected, Arc::clone(&registry), conn_id);
            }
            Err(e) => {
                // No further clients can be accepted; serve this one, then stop.
                error!(error = %e, "failed to create the next named pipe instance");
                spawn_conn(server, Arc::clone(&registry), conn_id);
                break;
            }
        }
    }

    shutdown(&registry, &pid_path).await;
    Ok(())
}

// ---- Connection spawn -------------------------------------------------------

fn spawn_conn(
    stream: tokio::net::windows::named_pipe::NamedPipeServer,
    registry: Arc<Mutex<Registry>>,
    conn_id: u64,
) {
    let (r, w) = tokio::io::split(stream);
    tokio::spawn(async move {
        conn::handle_conn(r, w, registry, conn_id).await;
    });
}

// ---- Shutdown ---------------------------------------------------------------

async fn shutdown(registry: &Arc<Mutex<Registry>>, pid_path: &Path) {
    // Capture final cwds and freeze the session list before killing children.
    registry.lock().unwrap().freeze_and_persist();

    // Shutdown: terminate each child → wait 2s → force-kill stragglers.
    let reg = Arc::clone(registry);
    let _ = tokio::task::spawn_blocking(move || Registry::shutdown_all(&reg)).await;
    // The pipe itself is a kernel object and disappears with the process; only
    // the pid file needs removing so `asd restart` does not chase a dead pid.
    let _ = std::fs::remove_file(pid_path);
    info!("asd daemon stopped");
}

// ---- Directory helpers ------------------------------------------------------

/// Named pipes don't need directory permissions; nothing to do.
pub(crate) fn prepare_socket_dir(_socket_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Named pipes are kernel objects — they disappear when the owning process
/// exits, so there is no stale file to clean up.
pub(crate) fn remove_stale_socket(_socket_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

// ---- Library loading --------------------------------------------------------

/// Constrain library loading to System32 before the first pty is created.
///
/// `portable-pty` opens the ConPTY entry points by probing a bare `conpty.dll`
/// (`win/psuedocon.rs`), meaning the default search order — application
/// directory first, then System32, then `PATH` — decides what gets loaded. A
/// `conpty.dll` dropped next to the executable, in the install directory or
/// anywhere on `PATH` would therefore be loaded into the daemon, which owns
/// every session's pty. Nothing here needs an application-local library at
/// runtime (`ghostty-vt.dll` is bound by the import table before `main`), so
/// the whole class goes away by resolving later loads against System32 only:
/// the sideload probe simply finds nothing and `portable-pty` falls back to
/// the ConPTY exports in the already-loaded `kernel32.dll`.
///
/// Do not add a dynamic, application-local library to the daemon without
/// revisiting this.
pub(crate) fn harden_dll_search() {
    if !win32::search_system32_only() {
        warn!("could not restrict the dll search path; a planted conpty.dll could be loaded");
    }
}

// ---- Process control --------------------------------------------------------

/// How long [`watch_child_exit`] lets the pty reader drain before it ends the
/// session. Output the child wrote just before exiting can still be sitting in
/// the ConPTY's buffer, and after the ending nothing reads it again.
const CHILD_EXIT_SETTLE: std::time::Duration = std::time::Duration::from_millis(100);

/// Minimal kernel32 process control (no extra crate).
mod win32 {
    unsafe extern "system" {
        fn SetDefaultDllDirectories(DirectoryFlags: u32) -> i32;
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> isize;
        fn TerminateProcess(hProcess: isize, uExitCode: u32) -> i32;
        fn CloseHandle(hObject: isize) -> i32;
        fn GenerateConsoleCtrlEvent(dwCtrlEvent: u32, dwProcessGroupId: u32) -> i32;
        fn WaitForSingleObject(hHandle: isize, dwMilliseconds: u32) -> u32;
    }

    /// Resolve bare library names against System32 only.
    const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;
    const PROCESS_TERMINATE: u32 = 0x0001;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const INFINITE: u32 = 0xFFFF_FFFF;
    const CTRL_BREAK_EVENT: u32 = 1;
    const INVALID_HANDLE_VALUE: isize = -1;

    /// Force-kill the process via TerminateProcess.
    pub fn force_kill(pid: u32) {
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        if handle != 0 && handle != INVALID_HANDLE_VALUE {
            unsafe {
                TerminateProcess(handle, 1);
                CloseHandle(handle);
            }
        }
    }

    /// Point the loader at System32 for every later load-by-name. Returns
    /// whether the policy took effect.
    pub fn search_system32_only() -> bool {
        unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) != 0 }
    }

    /// Block until the process exits. Returns at once when the pid cannot be
    /// opened, which for a child of this daemon means it is already gone: the
    /// handle is taken while the process is known to be alive, so nothing else
    /// can have claimed the number in between.
    pub fn wait_for_exit(pid: u32) {
        let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        if handle == 0 || handle == INVALID_HANDLE_VALUE {
            return;
        }
        unsafe {
            WaitForSingleObject(handle, INFINITE);
            CloseHandle(handle);
        }
    }

    /// Best-effort graceful stop, the nearest Windows analogue to SIGHUP.
    /// Returns whether the event was actually delivered.
    ///
    /// Two caveats make this fail far more often than it succeeds, so the
    /// caller must not assume the child is going away:
    /// `GenerateConsoleCtrlEvent`'s second argument is a process *group* id,
    /// not a pid, so it only reaches a child created with
    /// `CREATE_NEW_PROCESS_GROUP`; and it only reaches processes sharing the
    /// *caller's* console, which a ConPTY child of a daemon does not.
    pub fn graceful_kill(pid: u32) -> bool {
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0 }
    }
}

/// End the process `pid`: `force` skips straight to TerminateProcess.
///
/// A graceful stop that could not be delivered would otherwise leave the child
/// running until the 2s grace period expires and something else force-kills it —
/// so every `asd kill` would stall for two seconds and the child would still die
/// abruptly. Falling through costs the child nothing it was going to get anyway.
pub(crate) fn kill_child(pid: u32, force: bool) {
    if force || !win32::graceful_kill(pid) {
        win32::force_kill(pid);
    }
}

/// Watch for the child's exit and report it to the session thread, because the
/// pty will not: a ConPTY master stays readable for as long as the pseudoconsole
/// exists, and the daemon owns that until the session ends. Waiting for EOF is
/// therefore circular — the session would sit listed forever, still holding the
/// `OpenConsole.exe` whose exit it is waiting for, however its child died.
///
/// The wait runs on its own thread; it ends with the child, or with the session
/// if that goes first (the send then finds a dropped receiver, which is fine).
pub(crate) fn watch_child_exit(pid: u32, name: &str, tx: mpsc::Sender<SessionMsg>) {
    if pid == 0 {
        return;
    }
    let thread_name = format!("child-wait-{name}");
    let spawned = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            win32::wait_for_exit(pid);
            // The pty reader feeds the same channel, so everything it has
            // already taken is ordered ahead of this ending. The settle covers
            // what the child wrote last and the reader has not picked up yet.
            std::thread::sleep(CHILD_EXIT_SETTLE);
            let _ = tx.send(SessionMsg::Ended("child exited"));
        });
    if let Err(error) = spawned {
        warn!(session = %name, error = %error, "child-exit watch not started; the session will outlive its child");
    }
}

/// The cwd of a live process. Reading another process's cwd on Windows needs
/// `NtQueryInformationProcess` (undocumented but stable) or a toolhelp snapshot;
/// until that lands this returns `None`, so a restored session simply starts in
/// the daemon's default directory.
pub(crate) fn read_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

/// No fd to borrow: portable-pty exposes a HANDLE here, not a file descriptor,
/// and the caller only wants one to run `tcgetpgrp` against. `-1` is the same
/// "no answer" the unix side reports when the fd is unavailable, and the
/// Windows `foreground_command` ignores the value anyway.
pub(crate) fn pty_master_fd(_master: &(dyn portable_pty::MasterPty + Send)) -> i32 {
    -1
}
