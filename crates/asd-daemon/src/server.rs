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

/// How often the persisted session list re-reads each session's live cwd.
const CWD_REFRESH: std::time::Duration = std::time::Duration::from_secs(5);

/// Keep each session's recorded cwd current.
///
/// The list is otherwise only rewritten on create/rename/kill, and a session's
/// cwd at *create* time is not where it ends up: a shell asked to `cd` — whether
/// by `--cwd`, or by the user typing it later — has not moved yet when the
/// daemon samples it, so the entry records wherever the daemon itself was. It
/// used to stay wrong until some unrelated session was added or removed, or
/// until a clean shutdown; a crash in between persisted the wrong directory.
///
/// `persist` compares against what it last wrote, so a sweep that finds nothing
/// moved costs one readlink per session and no write at all.
fn spawn_cwd_refresh(registry: Arc<Mutex<Registry>>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(CWD_REFRESH);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            registry.lock().unwrap().persist();
        }
    });
}

/// Common serve path: load config, build the registry, restore persisted
/// sessions, then hand off to the platform-specific listener.
pub(super) async fn serve(socket_path: PathBuf) -> anyhow::Result<()> {
    let config = config::Config::load(&paths::config_path());
    let persist_path = paths::session_list_path();
    let registry = Arc::new(Mutex::new(Registry::new(
        config.scrollback_lines,
        persist_path.clone(),
        socket_path.clone(),
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

    spawn_cwd_refresh(Arc::clone(&registry));

    // Platform-specific listener + accept loop.
    crate::platform::serve_connections(socket_path, registry).await?;

    Ok(())
}
