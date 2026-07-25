//! Windows-specific daemon operations: Named Pipe listener, shutdown,
//! and stub directory helpers.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tracing::{error, info};

use crate::conn;
use crate::registry::Registry;

// ---- Listener ---------------------------------------------------------------

pub(super) async fn serve_connections(
    pipe_path: PathBuf,
    registry: Arc<Mutex<Registry>>,
) -> anyhow::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let pipe_name = pipe_path
        .to_str()
        .context("pipe path is not valid UTF-8")?
        .to_string();
    info!(pipe = %pipe_name, version = env!("CARGO_PKG_VERSION"), "asd daemon listening");

    let mut conn_id: u64 = 0;
    loop {
        // Create a new server instance for each connection.
        // NamedPipeServer on Windows: each instance serves one client; the
        // next client waits until we create another instance.
        let server = match ServerOptions::new()
            .first_pipe_instance(false)
            .create(&pipe_name)
        {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "failed to create named pipe server");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
        };

        tokio::select! {
            result = server.connect() => {
                match result {
                    Ok(()) => {
                        conn_id += 1;
                        spawn_conn(server, Arc::clone(&registry), conn_id);
                    }
                    Err(e) => {
                        error!(error = %e, "named pipe connect failed");
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C received, shutting down");
                break;
            }
        }
    }

    shutdown(&registry).await;
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

async fn shutdown(registry: &Arc<Mutex<Registry>>) {
    // Capture final cwds and freeze the session list before killing children.
    registry.lock().unwrap().freeze_and_persist();

    // Shutdown: terminate each child → wait 2s → force-kill stragglers.
    let reg = Arc::clone(registry);
    let _ = tokio::task::spawn_blocking(move || Registry::shutdown_all(&reg)).await;
    // Named pipes auto-cleanup when the process exits; nothing to remove.
    info!("asd daemon stopped");
}

// ---- Directory helpers ------------------------------------------------------

/// Named pipes don't need directory permissions; nothing to do.
pub(super) fn prepare_socket_dir(_socket_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Named pipes are kernel objects — they disappear when the owning process
/// exits, so there is no stale file to clean up.
pub(super) fn remove_stale_socket(_socket_path: &Path) -> anyhow::Result<()> {
    Ok(())
}
