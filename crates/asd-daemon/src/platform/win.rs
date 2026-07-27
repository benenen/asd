//! Windows-specific daemon operations: Named Pipe listener, shutdown, process
//! control. Selected by [`super`]; see there for the shared surface.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use asd_proto::paths;
use tracing::{error, info, warn};

use crate::conn;
use crate::registry::Registry;

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

// ---- Process control --------------------------------------------------------

/// Minimal kernel32 process control (no extra crate).
mod win32 {
    unsafe extern "system" {
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> isize;
        fn TerminateProcess(hProcess: isize, uExitCode: u32) -> i32;
        fn CloseHandle(hObject: isize) -> i32;
        fn GenerateConsoleCtrlEvent(dwCtrlEvent: u32, dwProcessGroupId: u32) -> i32;
    }

    const PROCESS_TERMINATE: u32 = 0x0001;
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
