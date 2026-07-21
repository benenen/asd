//! asd daemon: headless mux (spec §5), shipped as a library.
//!
//! There is no separate daemon binary: the `asd` CLI embeds this crate and
//! runs it via the `asd daemon` subcommand (single-binary distribution).
//! Keeping the daemon in its own crate preserves the spec §3 dependency
//! boundary: no iced/wgpu, including transitively.
//!
//! [`run`] serves in the foreground; detaching (setsid, redirecting logs to
//! the data directory) is the spawner's responsibility (the self-healing
//! path of `asd attach -A` / `asd new`).

mod config;
mod conn;
mod registry;
mod session;
mod store;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use asd_proto::paths;
use registry::Registry;
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info, warn};

/// Run the daemon until SIGTERM/SIGINT. Blocks the calling thread and owns
/// its own tokio runtime; call from a plain (non-async) context.
///
/// `socket` defaults to `$ASD_SOCKET`, then `$XDG_RUNTIME_DIR/asd.sock`.
pub fn run(socket: Option<PathBuf>) -> anyhow::Result<()> {
    // try_init: the embedding binary may have installed a subscriber already
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let socket_path = socket.unwrap_or_else(paths::socket_path);

    // Data directory (the spawner redirects logs here; session metadata uses
    // it from M1 on)
    std::fs::create_dir_all(paths::data_dir()).context("creating data dir")?;

    prepare_socket_dir(&socket_path)?;
    remove_stale_socket(&socket_path)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(socket_path))
}

async fn serve(socket_path: PathBuf) -> anyhow::Result<()> {
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;
    info!(socket = %socket_path.display(), version = env!("CARGO_PKG_VERSION"), "asd daemon listening");

    // Record our pid next to the socket so `asd restart` can stop us by signal
    // (removed on clean shutdown below).
    let pid_path = paths::pid_path(&socket_path);
    if let Err(e) = std::fs::write(&pid_path, std::process::id().to_string()) {
        warn!(error = %e, "failed to write pid file");
    }

    // Load config (scrollback depth, …) once at startup; a missing/broken file
    // falls back to defaults. It governs every session this daemon spawns —
    // change it and `asd restart` to apply.
    let config = config::Config::load(&paths::config_path());
    let persist_path = paths::session_list_path();
    let registry = Arc::new(Mutex::new(Registry::new(
        config.scrollback_lines,
        persist_path.clone(),
    )));

    // Restore the persisted session list on every startup (fresh boot, crash
    // recovery, or `asd restart`): recreate each saved session as a fresh shell
    // `cd`'d to its saved cwd. Each create re-persists the file.
    for st in store::read(&persist_path) {
        match Registry::create(&registry, Some(st.name.clone()), None, st.cwd) {
            Ok(_) => info!(session = %st.name, "session restored"),
            Err((code, msg)) => warn!(session = %st.name, code, %msg, "restore failed"),
        }
    }

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    // `asd restart` sends SIGUSR1: shut down (the session list is already kept
    // up to date on disk, so no special handling is needed here).
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

    // Capture final cwds and freeze the session list before killing children:
    // the SIGHUP-driven `Registry::remove` calls below must not erase the file,
    // so the next daemon can restore from it.
    registry.lock().unwrap().freeze_and_persist();

    // Shutdown contract (spec §5): SIGHUP each child → wait 2s → SIGKILL
    // stragglers → remove socket.
    let reg = Arc::clone(&registry);
    tokio::task::spawn_blocking(move || Registry::shutdown_all(&reg)).await?;
    if let Err(e) = std::fs::remove_file(&socket_path) {
        warn!(error = %e, "failed to remove socket file");
    }
    let _ = std::fs::remove_file(&pid_path);
    info!("asd daemon stopped");
    Ok(())
}

fn spawn_conn(stream: UnixStream, registry: Arc<Mutex<Registry>>, conn_id: u64) {
    tokio::spawn(async move {
        conn::handle_conn(stream, registry, conn_id).await;
    });
}

/// Ensure the socket directory exists; the fallback directory
/// (/tmp/asd-$UID) must be 0700.
fn prepare_socket_dir(socket_path: &std::path::Path) -> anyhow::Result<()> {
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
fn remove_stale_socket(socket_path: &std::path::Path) -> anyhow::Result<()> {
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
