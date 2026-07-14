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

mod conn;
mod registry;
mod session;

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

    let registry = Arc::new(Mutex::new(Registry::default()));

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

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
        }
    }

    // Shutdown contract (spec §5): SIGHUP each child → wait 2s → SIGKILL
    // stragglers → remove socket.
    // Sessions are not persisted: restarting the daemon = all sessions gone
    // (explicitly so in v1).
    let reg = Arc::clone(&registry);
    tokio::task::spawn_blocking(move || Registry::shutdown_all(&reg)).await?;
    if let Err(e) = std::fs::remove_file(&socket_path) {
        warn!(error = %e, "failed to remove socket file");
    }
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
