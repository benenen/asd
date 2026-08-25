//! Session registry: daemon-wide unique naming, create/list/kill.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::session::kill_child;
use asd_proto::{SessionInfo, code, paths};
use tracing::info;

use crate::detect::Detector;
use crate::session::{SessionContext, SessionHandle, SessionMsg, spawn_session};

/// Default terminal size for a create without dimensions (immediately
/// overridden by the client's size on attach).
const DEFAULT_SIZE: (u16, u16) = (80, 24);

pub struct Registry {
    sessions: HashMap<String, SessionHandle>,
    /// Auto-naming counter for `s0`, `s1`, ... — monotonically increasing
    /// (avoids reusing a name that just died).
    next_auto: u64,
    /// Scrollback depth (lines) applied to every session this registry spawns;
    /// comes from the daemon config, resolved once at startup.
    scrollback_lines: usize,
    /// Where the live session list is persisted; rewritten on every mutation.
    persist_path: PathBuf,
    /// What every session this registry spawns needs from the daemon: its
    /// listener (handed to each child as `$ASD_SOCKET`, so an `asd` command run
    /// inside a session addresses the daemon hosting it) and the shared
    /// agent-detection rules.
    context: SessionContext,
    /// Once set (at shutdown), `persist` is a no-op — so the SIGHUP-driven
    /// session removals during shutdown don't wipe the file before restart.
    persist_frozen: bool,
    /// What was last written to `persist_path`, so the periodic cwd refresh can
    /// skip the write when nothing moved.
    last_persisted: Vec<crate::store::SessionState>,
    /// The sampler's most recent reading and when it was taken. `None` until
    /// its first tick.
    host_metrics: Option<(asd_proto::HostSample, std::time::Instant)>,
}

impl Registry {
    /// Create an empty registry whose sessions each keep `scrollback_lines` lines
    /// of scrollback, whose live set is persisted to `persist_path`, and whose
    /// children are pointed at `socket_path`.
    pub fn new(scrollback_lines: usize, persist_path: PathBuf, socket_path: PathBuf) -> Self {
        Self {
            sessions: HashMap::new(),
            next_auto: 0,
            scrollback_lines,
            persist_path,
            context: SessionContext {
                socket: socket_path,
                // Loaded once per daemon: the rules are the same for every
                // session, and re-reading the config directory per spawn would
                // let two sessions started minutes apart disagree.
                detector: Arc::new(Detector::load(Some(&paths::agents_dir()))),
            },
            persist_frozen: false,
            last_persisted: Vec::new(),
            host_metrics: None,
        }
    }

    /// Create a session. `name` defaults to auto-assignment; `cmd` defaults
    /// to `$SHELL`, and is both what the child runs and what is persisted.
    pub fn create(
        registry: &Arc<Mutex<Self>>,
        name: Option<String>,
        cmd: Option<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<String, (u32, String)> {
        Self::spawn(registry, name, cmd.clone(), cmd, cwd)
    }

    /// Recreate a session the persisted list remembers, with its recorded
    /// `command` *staged rather than run*: the child is a plain shell in `cwd`,
    /// and the command is only what the daemon writes at that shell's prompt
    /// afterwards (see `server::stage_restored_command`).
    ///
    /// A restart must not re-run an arbitrary command on its own — the recorded
    /// command could be a migration, a deploy, or anything else whose second
    /// run is not free. The session still carries the command forward, so it
    /// survives the *next* restart too.
    pub fn restore(
        registry: &Arc<Mutex<Self>>,
        name: String,
        command: Option<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<String, (u32, String)> {
        Self::spawn(registry, Some(name), None, command, cwd)
    }

    /// The one spawn path. `run` is what the child executes (`None` = the
    /// default shell); `record` is what the persisted list remembers, which is
    /// the same thing for an ordinary create and the staged command for a
    /// restore.
    fn spawn(
        registry: &Arc<Mutex<Self>>,
        name: Option<String>,
        run: Option<String>,
        record: Option<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<String, (u32, String)> {
        let mut reg = registry.lock().unwrap();
        let name = match name {
            Some(n) => {
                if !paths::is_valid_session_name(&n) {
                    return Err((
                        code::INVALID_NAME,
                        format!("invalid session name '{n}' (want [A-Za-z0-9_-]{{1,64}})"),
                    ));
                }
                if reg.sessions.contains_key(&n) {
                    return Err((
                        code::SESSION_EXISTS,
                        format!("session '{n}' already exists"),
                    ));
                }
                n
            }
            None => loop {
                let candidate = format!("s{}", reg.next_auto);
                reg.next_auto += 1;
                if !reg.sessions.contains_key(&candidate) {
                    break candidate;
                }
            },
        };

        let scrollback = reg.scrollback_lines;
        let context = reg.context.clone();
        let mut handle = spawn_session(
            name.clone(),
            run,
            cwd,
            DEFAULT_SIZE.0,
            DEFAULT_SIZE.1,
            scrollback,
            context,
            Arc::clone(registry),
        )
        .map_err(|e| (code::INTERNAL, format!("failed to spawn session: {e}")))?;
        handle.spawn_command = record;
        reg.sessions.insert(name.clone(), handle);
        reg.persist();
        info!(session = %name, "session created");
        Ok(name)
    }

    /// Snapshot each live session's name, cwd, and recorded command for
    /// persistence/restore. Reads `/proc/<pid>/cwd` under the lock — a cheap
    /// readlink.
    pub fn snapshot(&self) -> Vec<crate::store::SessionState> {
        self.sessions
            .values()
            .map(|h| {
                let name = h
                    .meta
                    .name
                    .lock()
                    .map(|n| n.clone())
                    .unwrap_or_else(|_| h.name.clone());
                let pid = h.meta.child_pid.load(std::sync::atomic::Ordering::Relaxed);
                crate::store::SessionState {
                    name,
                    cwd: crate::store::read_cwd(pid),
                    command: h.spawn_command.clone(),
                }
            })
            .collect()
    }

    /// Rewrite the persisted session list from the live set (no-op while frozen).
    /// Also called once after startup restore to compact the file down to the
    /// sessions that actually came back.
    pub fn persist(&mut self) {
        if self.persist_frozen {
            return;
        }
        let snap = self.snapshot();
        // A session's cwd is read live, so most refreshes find nothing changed;
        // comparing first keeps the periodic sweep from rewriting the file every
        // few seconds for no reason.
        if snap == self.last_persisted {
            return;
        }
        crate::store::write_atomic(&self.persist_path, &snap);
        self.last_persisted = snap;
    }

    /// Final persist (capturing live cwds), then freeze so the shutdown SIGHUPs'
    /// session removals don't clobber the file. Called once on the way out.
    pub fn freeze_and_persist(&mut self) {
        let snap = self.snapshot();
        crate::store::write_atomic(&self.persist_path, &snap);
        self.last_persisted = snap;
        self.persist_frozen = true;
    }

    pub fn get(&self, name: &str) -> Option<SessionHandle> {
        self.sessions.get(name).cloned()
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let mut infos: Vec<_> = self.sessions.values().map(SessionHandle::info).collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    /// Callback at the session thread's endpoint: deregister and re-persist (so a
    /// killed or self-exited session drops off the list). A no-op on the file
    /// during shutdown, where `persist_frozen` is set.
    pub fn remove(&mut self, name: &str) {
        self.sessions.remove(name);
        self.persist();
    }

    /// Rename `old` to `new`: validate the new name, move the map key, and
    /// update the session's canonical name in `meta` (so `info()` and the
    /// session thread's self-removal follow it).
    pub fn rename(&mut self, old: &str, new: &str) -> Result<(), (u32, String)> {
        if !paths::is_valid_session_name(new) {
            return Err((
                code::INVALID_NAME,
                format!("invalid session name '{new}' (want [A-Za-z0-9_-]{{1,64}})"),
            ));
        }
        if new == old {
            return Ok(()); // no-op rename to the same name
        }
        if self.sessions.contains_key(new) {
            return Err((
                code::SESSION_EXISTS,
                format!("session '{new}' already exists"),
            ));
        }
        let Some(handle) = self.sessions.remove(old) else {
            return Err((code::NO_SUCH_SESSION, format!("no such session '{old}'")));
        };
        if let Ok(mut n) = handle.meta.name.lock() {
            *n = new.to_string();
        }
        let _ = handle.tx.send(SessionMsg::ViewRenamed {
            old_name: old.to_string(),
            new_name: new.to_string(),
        });
        self.sessions.insert(new.to_string(), handle);
        self.persist();
        info!(from = %old, to = %new, "session renamed");
        Ok(())
    }

    pub fn kill(&self, name: &str) -> Result<(), (u32, String)> {
        match self.sessions.get(name) {
            Some(h) => {
                let _ = h.tx.send(SessionMsg::Kill);
                Ok(())
            }
            None => Err((code::NO_SUCH_SESSION, format!("no such session '{name}'"))),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Shutdown (spec §5): SIGHUP each session's child → wait 2s → SIGKILL
    /// stragglers. Blocking version, called only on the daemon exit path.
    pub fn shutdown_all(registry: &Arc<Mutex<Self>>) {
        let handles: Vec<SessionHandle> = registry
            .lock()
            .unwrap()
            .sessions
            .values()
            .cloned()
            .collect();
        if handles.is_empty() {
            return;
        }
        info!(count = handles.len(), "shutting down sessions (SIGHUP)");
        for h in &handles {
            kill_child(&h.meta, false);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if registry.lock().unwrap().is_empty() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        info!("grace period over, SIGKILL remaining children");
        for h in &handles {
            kill_child(&h.meta, true);
        }
        // Give the EOF→reap path a moment, to avoid leaving zombies for init
        // to adopt
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            if registry.lock().unwrap().is_empty() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Store a fresh reading from the sampler.
    pub fn set_host_metrics(&mut self, sample: asd_proto::HostSample) {
        self.host_metrics = Some((sample, std::time::Instant::now()));
    }

    /// The latest reading with its age filled in. The age is computed here, at
    /// read time, so it measures how stale the reading is when it reaches a
    /// client rather than when it was stored.
    pub fn host_metrics(&self) -> Option<asd_proto::HostSample> {
        self.host_metrics.map(|(sample, at)| asd_proto::HostSample {
            sampled_age_ms: u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX),
            ..sample
        })
    }
}
