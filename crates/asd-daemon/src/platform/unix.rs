//! Unix-specific daemon operations: UDS listener, signal handling,
//! socket-directory management, process control. Selected by [`super`]; see
//! there for the shared surface.

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
    socket_path: PathBuf,
    registry: Arc<Mutex<Registry>>,
) -> anyhow::Result<()> {
    let listener = tokio::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;
    info!(socket = %socket_path.display(), version = env!("CARGO_PKG_VERSION"), "asd daemon listening");

    // Record our pid next to the socket so `asd restart` can stop us by signal.
    let pid_path = paths::pid_path(&socket_path);
    if let Err(e) = std::fs::write(&pid_path, std::process::id().to_string()) {
        warn!(error = %e, "failed to write pid file");
    }

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigusr1 =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;

    let mut conn_id: u64 = 0;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _addr)) => {
                    conn_id += 1;
                    spawn_conn(stream, Arc::clone(&registry), conn_id);
                }
                Err(e) => {
                    error!(error = %e, "accept failed");
                }
            },
            _ = sigterm.recv() => { info!("SIGTERM received"); break; }
            _ = sigint.recv() => { info!("SIGINT received"); break; }
            _ = sigusr1.recv() => { info!("SIGUSR1 received"); break; }
        }
    }

    shutdown(&registry, &socket_path, &pid_path).await;
    Ok(())
}

// ---- Connection spawn -------------------------------------------------------

fn spawn_conn(stream: tokio::net::UnixStream, registry: Arc<Mutex<Registry>>, conn_id: u64) {
    let (r, w) = stream.into_split();
    tokio::spawn(async move {
        conn::handle_conn(r, w, registry, conn_id).await;
    });
}

// ---- Shutdown ---------------------------------------------------------------

async fn shutdown(registry: &Arc<Mutex<Registry>>, socket_path: &Path, pid_path: &Path) {
    // Capture final cwds and freeze the session list before killing children.
    registry.lock().unwrap().freeze_and_persist();

    // Shutdown: SIGHUP each child → wait 2s → SIGKILL stragglers → remove socket.
    let reg = Arc::clone(registry);
    let _ = tokio::task::spawn_blocking(move || Registry::shutdown_all(&reg)).await;
    if let Err(e) = std::fs::remove_file(socket_path) {
        warn!(error = %e, "failed to remove socket file");
    }
    let _ = std::fs::remove_file(pid_path);
    info!("asd daemon stopped");
}

// ---- Directory helpers ------------------------------------------------------

/// Ensure the socket directory exists; the fallback directory
/// (/tmp/asd-$UID) must be 0700.
pub(crate) fn prepare_socket_dir(socket_path: &Path) -> anyhow::Result<()> {
    let Some(dir) = socket_path.parent() else {
        return Ok(());
    };
    if dir.as_os_str().is_empty() || dir.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let perms = std::os::unix::fs::PermissionsExt::from_mode(0o700);
    std::fs::set_permissions(dir, perms)?;
    Ok(())
}

/// Stale socket handling: if it accepts a connection, a daemon is already
/// running (error out); if it refuses (ECONNREFUSED), it is a corpse from a
/// previous crash — remove it and rebind.
pub(crate) fn remove_stale_socket(socket_path: &Path) -> anyhow::Result<()> {
    if !socket_path.exists() {
        return Ok(());
    }
    match std::os::unix::net::UnixStream::connect(socket_path) {
        Ok(_) => anyhow::bail!(
            "another daemon is already listening on {}",
            socket_path.display()
        ),
        Err(_) => {
            warn!(socket = %socket_path.display(), "removing stale socket");
            std::fs::remove_file(socket_path).context("removing stale socket")?;
            Ok(())
        }
    }
}

// ---- Process control --------------------------------------------------------

/// End the process `pid`: `force` picks SIGKILL over SIGHUP. A failure means it
/// is already gone, which is indistinguishable from success here.
pub(crate) fn kill_child(pid: u32, force: bool) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let sig = if force {
        Signal::SIGKILL
    } else {
        Signal::SIGHUP
    };
    let _ = kill(Pid::from_raw(pid as i32), sig);
}

/// Watch for the child's exit. Nothing to do here: the child's exit closes the
/// last slave fd, the master read returns EOF, and the pty reader reports the
/// ending already. A watcher would also make a second party interested in the
/// pid, which only `Child::wait` on the session thread may reap.
pub(crate) fn watch_child_exit(_pid: u32, _name: &str, _tx: mpsc::Sender<SessionMsg>) {}

/// The cwd of a live process, read from `/proc/<pid>/cwd`. `None` on any error —
/// the session then recreates in the daemon's default directory rather than
/// failing.
pub(crate) fn read_cwd(pid: u32) -> Option<PathBuf> {
    if pid == 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// The pty master's raw fd, borrowed (not duplicated) — the master owns it and
/// keeps it open for the session's lifetime. `-1` when it cannot be obtained,
/// which `foreground_command` treats as "no answer".
pub(crate) fn pty_master_fd(master: &(dyn portable_pty::MasterPty + Send)) -> i32 {
    master.as_raw_fd().unwrap_or(-1)
}
