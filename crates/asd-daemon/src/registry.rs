//! Session registry: daemon-wide unique naming, create/list/kill.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use asd_proto::{SessionInfo, code, paths};
use nix::sys::signal::Signal;
use tracing::info;

use crate::session::{SessionHandle, SessionMsg, spawn_session};

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
    /// Once set (at shutdown), `persist` is a no-op — so the SIGHUP-driven
    /// session removals during shutdown don't wipe the file before restart.
    persist_frozen: bool,
}

impl Registry {
    /// Create an empty registry whose sessions each keep `scrollback_lines` lines
    /// of scrollback and whose live set is persisted to `persist_path`.
    pub fn new(scrollback_lines: usize, persist_path: PathBuf) -> Self {
        Self {
            sessions: HashMap::new(),
            next_auto: 0,
            scrollback_lines,
            persist_path,
            persist_frozen: false,
        }
    }

    /// Create a session. `name` defaults to auto-assignment; `cmd` defaults
    /// to `$SHELL`.
    pub fn create(
        registry: &Arc<Mutex<Self>>,
        name: Option<String>,
        cmd: Option<String>,
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
        let handle = spawn_session(
            name.clone(),
            cmd,
            cwd,
            DEFAULT_SIZE.0,
            DEFAULT_SIZE.1,
            scrollback,
            Arc::clone(registry),
        )
        .map_err(|e| (code::INTERNAL, format!("failed to spawn session: {e}")))?;
        reg.sessions.insert(name.clone(), handle);
        reg.persist();
        info!(session = %name, "session created");
        Ok(name)
    }

    /// Snapshot each live session's name + cwd for persistence/restore. Reads
    /// `/proc/<pid>/cwd` under the lock — a cheap readlink.
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
                }
            })
            .collect()
    }

    /// Rewrite the persisted session list from the live set (no-op while frozen).
    /// Also called once after startup restore to compact the file down to the
    /// sessions that actually came back.
    pub fn persist(&self) {
        if self.persist_frozen {
            return;
        }
        crate::store::write_atomic(&self.persist_path, &self.snapshot());
    }

    /// Final persist (capturing live cwds), then freeze so the shutdown SIGHUPs'
    /// session removals don't clobber the file. Called once on the way out.
    pub fn freeze_and_persist(&mut self) {
        crate::store::write_atomic(&self.persist_path, &self.snapshot());
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
            crate::session::signal_child(&h.meta, Signal::SIGHUP);
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
            crate::session::signal_child(&h.meta, Signal::SIGKILL);
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
}
