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
mod detect;
mod metrics;
mod platform;
mod registry;
mod server;
mod session;
mod store;

pub use store::read_cwd;

use std::path::PathBuf;

use anyhow::Context;
use asd_proto::paths;

/// Run the daemon until SIGTERM/SIGINT. Blocks the calling thread and owns
/// its own tokio runtime; call from a plain (non-async) context.
///
/// `socket` defaults to `$ASD_SOCKET`, then `$XDG_RUNTIME_DIR/asd.sock`.
/// `run_restored_commands` forces restored commands to run instead of waiting
/// at their prompt, overriding the config file's `session.run_restored_commands`
/// for this daemon only.
pub fn run(socket: Option<PathBuf>, run_restored_commands: bool) -> anyhow::Result<()> {
    // try_init: the embedding binary may have installed a subscriber already
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .try_init();

    // Before anything can load a library by name — the first pty is what
    // triggers it — decide where libraries may come from.
    platform::harden_dll_search();

    let socket_path = socket.unwrap_or_else(paths::socket_path);

    // Data directory (the spawner redirects logs here; session metadata uses
    // it from M1 on)
    std::fs::create_dir_all(paths::data_dir()).context("creating data dir")?;

    platform::prepare_socket_dir(&socket_path)?;
    platform::remove_stale_socket(&socket_path)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(server::serve(socket_path, run_restored_commands))
}
