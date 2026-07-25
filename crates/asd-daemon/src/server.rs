//! Daemon startup orchestration: config loading, registry creation, session
//! restore, and the platform-specific listener/accept loop.
//!
//! The platform-specific connection-serving logic lives in [`super::unix`] and
//! [`super::win`]; this module is the common path that runs before and after.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use asd_proto::paths;
use tracing::{info, warn};

use crate::config;
use crate::registry::Registry;
use crate::store;

/// Common serve path: load config, build the registry, restore persisted
/// sessions, then hand off to the platform-specific listener.
pub(super) async fn serve(socket_path: PathBuf) -> anyhow::Result<()> {
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
    // Compact the file down to what actually came back. Each successful restore
    // above already re-persists, but a boot where every entry failed to restore
    // (e.g. a stale hand-edited file) would otherwise leave the bad entries on
    // disk and retry them forever — this one write drops them.
    registry.lock().unwrap().persist();

    // Platform-specific listener + accept loop.
    #[cfg(unix)]
    crate::unix::serve_connections(socket_path, registry).await?;
    #[cfg(windows)]
    crate::win::serve_connections(socket_path, registry).await?;

    Ok(())
}
