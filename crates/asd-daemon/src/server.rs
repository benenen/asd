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

/// How long a restored session's shell gets to come up before its recorded
/// command is typed at the prompt.
///
/// The shell reads the bytes either way — a pty holds them until something
/// reads — but the tty echoes them as they arrive, so writing before the prompt
/// is drawn leaves the command sitting above it. This is a heuristic for
/// appearance only: nothing is lost if a slow rc file outlasts it.
const STAGE_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

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

/// Type a restored session's recorded command at its shell prompt, without the
/// newline that would run it.
///
/// This is the whole of the "restore brings the command back, staged" contract:
/// the session is an ordinary shell, and what makes it a restore is that the
/// command is waiting on the prompt line — editable, discardable with Ctrl+C,
/// and run only when someone presses Enter. `run` sends the newline as well,
/// for a daemon started with `--run-restored-commands` or a config that asks
/// for it.
fn stage_restored_command(
    registry: Arc<Mutex<Registry>>,
    name: String,
    command: String,
    run: bool,
) {
    tokio::spawn(async move {
        tokio::time::sleep(STAGE_DELAY).await;
        // Take the handle and drop the lock before awaiting anything.
        let handle = {
            let reg = registry.lock().unwrap();
            reg.get(&name)
        };
        let Some(handle) = handle else {
            return; // the session ended before it could be staged
        };
        let (completed, applied) = tokio::sync::oneshot::channel();
        let sent = handle.tx.send(crate::session::SessionMsg::ScriptInput {
            bytes: command.into_bytes(),
            enter: run,
            completed,
        });
        if sent.is_err() {
            warn!(session = %name, "session ended before its command could be staged");
            return;
        }
        match applied.await {
            Ok(Ok(())) => info!(session = %name, run, "restored command staged"),
            Ok(Err(error)) => warn!(session = %name, %error, "staging the restored command failed"),
            Err(_) => warn!(session = %name, "session ended while staging its command"),
        }
    });
}

/// Common serve path: load config, build the registry, restore persisted
/// sessions, then hand off to the platform-specific listener.
///
/// `force_run_commands` overrides the config for this daemon: see
/// [`stage_restored_command`].
pub(super) async fn serve(socket_path: PathBuf, force_run_commands: bool) -> anyhow::Result<()> {
    let config = config::Config::load(&paths::config_path());
    let persist_path = paths::session_list_path();
    let registry = Arc::new(Mutex::new(Registry::new(
        config.scrollback_lines,
        persist_path.clone(),
        socket_path.clone(),
    )));

    // Restore the persisted session list on every startup (fresh boot, crash
    // recovery, or `asd restart`): recreate each saved session as a fresh shell
    // `cd`'d to its saved cwd, with the command it was created with typed at
    // that shell's prompt but not run. Each create re-persists the file.
    let run_commands = force_run_commands || config.run_restored_commands;
    for st in store::read(&persist_path) {
        match Registry::restore(&registry, st.name.clone(), st.command.clone(), st.cwd) {
            Ok(name) => {
                info!(session = %st.name, staged = st.command.is_some(), "session restored");
                if let Some(command) = st.command {
                    stage_restored_command(Arc::clone(&registry), name, command, run_commands);
                }
            }
            Err((code, msg)) => warn!(session = %st.name, code, %msg, "restore failed"),
        }
    }
    // Compact the file down to what actually came back. Each successful restore
    // above already re-persists, but a boot where every entry failed to restore
    // (e.g. a stale hand-edited file) would otherwise leave the bad entries on
    // disk and retry them forever — this one write drops them.
    registry.lock().unwrap().persist();

    spawn_cwd_refresh(Arc::clone(&registry));
    crate::metrics::spawn(Arc::clone(&registry));

    // Platform-specific listener + accept loop.
    crate::platform::serve_connections(socket_path, registry).await?;

    Ok(())
}
