//! Session registry: daemon-wide unique naming, create/list/kill.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::session::kill_child;
use asd_proto::{SessionIdentity, SessionInfo, code, paths};
use tracing::info;

use crate::detect::DetectorStore;
use crate::session::{SessionContext, SessionHandle, SessionMsg, spawn_session};

/// Default terminal size for a create without dimensions (immediately
/// overridden by the client's size on attach).
const DEFAULT_SIZE: (u16, u16) = (80, 24);

mod reload;

pub struct Registry {
    pub events: crate::event_hub::EventPublisher,
    detectors: DetectorStore,
    reload_gate: Arc<tokio::sync::Mutex<()>>,
    sessions: HashMap<String, SessionHandle>,
    agent_records: HashMap<SessionIdentity, crate::agent_resume::AgentResumeRecord>,
    /// Auto-naming counter for `s0`, `s1`, ... — monotonically increasing
    /// (avoids reusing a name that just died).
    next_auto: u64,
    /// Scrollback depth (lines) applied to every session this registry spawns;
    /// comes from the daemon config, resolved once at startup.
    scrollback_lines: usize,
    /// The registry-owned serialized writer for durable session state.
    store: crate::store::SessionStore,
    /// Restored records whose shells could not be recreated. They remain in
    /// durable state for a later daemon start instead of being compacted away.
    unrestored: Vec<crate::store::SessionState>,
    /// What every session this registry spawns needs from the daemon: its
    /// listener (handed to each child as `$ASD_SOCKET`, so an `asd` command run
    /// inside a session addresses the daemon hosting it) and the shared
    /// agent-detection rules.
    context: SessionContext,
    /// Once set (at shutdown), `persist` is a no-op — so the SIGHUP-driven
    /// session removals during shutdown don't wipe the file before restart.
    persist_frozen: bool,
    /// What was last written to the store, so the periodic cwd refresh can
    /// skip the write when nothing moved.
    last_persisted: Vec<crate::store::SessionState>,
    /// The sampler's most recent reading and when it was taken. `None` until
    /// its first tick.
    host_metrics: Option<(asd_proto::HostSample, std::time::Instant)>,
}

impl Registry {
    /// Create a registry whose sessions each keep `scrollback_lines` lines of
    /// scrollback and whose children are pointed at `socket_path`.
    pub fn new(
        scrollback_lines: usize,
        store: crate::store::SessionStore,
        unrestored: Vec<crate::store::SessionState>,
        socket_path: PathBuf,
    ) -> anyhow::Result<Self> {
        let detectors = DetectorStore::load(paths::agents_dir());
        let events = crate::event_hub::EventPublisher::new()?;
        Ok(Self {
            events: events.clone(),
            sessions: HashMap::new(),
            agent_records: HashMap::new(),
            next_auto: 0,
            scrollback_lines,
            store,
            unrestored,
            context: SessionContext {
                socket: socket_path,
                detector: detectors.snapshot(),
                events,
            },
            detectors,
            reload_gate: Arc::new(tokio::sync::Mutex::new(())),
            persist_frozen: false,
            last_persisted: Vec::new(),
            host_metrics: None,
        })
    }

    /// Create a session. `name` defaults to auto-assignment; `cmd` defaults
    /// to `$SHELL`, and is both what the child runs and what is persisted.
    pub fn create(
        registry: &Arc<Mutex<Self>>,
        name: Option<String>,
        cmd: Option<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<String, (u32, String)> {
        Self::spawn(registry, name, cmd.clone(), cmd, cwd, false)
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
        if let Some(path) = cwd.as_ref()
            && !path.is_dir()
        {
            return Err((
                code::INTERNAL,
                format!(
                    "failed to restore session '{name}': cwd {} is not a directory",
                    path.display()
                ),
            ));
        }
        Self::spawn(registry, Some(name), None, command, cwd, true)
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
        restoring: bool,
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
        let (mut handle, registered) = spawn_session(
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
        if restoring
            && let Some(record) = reg
                .unrestored
                .iter()
                .find(|state| state.name == name)
                .and_then(|state| state.agent_resume.clone())
        {
            reg.agent_records.insert(handle.identity(), record);
        }
        let info = handle.info();
        reg.events
            .track_activity(handle.identity(), Arc::clone(&handle.meta));
        reg.sessions.insert(name.clone(), handle);
        reg.events
            .publish(crate::event_hub::CommittedSessionUpdate::Registered(info));
        let _ = registered.send(());
        // A successful explicit create replaces a retained failed restore.
        // Only restoration may transfer authoritative conversation metadata.
        if !restoring {
            reg.unrestored.retain(|state| state.name != name);
        }
        reg.persist();
        info!(session = %name, "session created");
        Ok(name)
    }

    /// Snapshot each live session's name, cwd, and recorded command for
    /// persistence/restore. Reads `/proc/<pid>/cwd` under the lock — a cheap
    /// readlink.
    pub fn snapshot(&self) -> Vec<crate::store::SessionState> {
        let mut states: Vec<_> = self
            .sessions
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
                    agent_resume: self.agent_records.get(&h.identity()).cloned(),
                }
            })
            .collect();
        states.sort_by(|left, right| left.name.cmp(&right.name));
        states
    }

    fn persisted_state(&self) -> Vec<crate::store::SessionState> {
        let mut states: BTreeMap<String, crate::store::SessionState> = self
            .unrestored
            .iter()
            .cloned()
            .map(|state| (state.name.clone(), state))
            .collect();
        for state in self.snapshot() {
            states.insert(state.name.clone(), state);
        }
        states.into_values().collect()
    }

    /// Rewrite the persisted session list from the live set (no-op while frozen).
    /// Also called once after startup restore to compact the file down to the
    /// sessions that actually came back.
    pub fn persist(&mut self) {
        if self.persist_frozen {
            return;
        }
        let snap = self.persisted_state();
        // A session's cwd is read live, so most refreshes find nothing changed;
        // comparing first keeps the periodic sweep from rewriting the file every
        // few seconds for no reason.
        if snap == self.last_persisted {
            if let Err(error) = self.store.retry_dirty(&snap) {
                tracing::warn!(error = %error, "failed to retry session-store persistence");
            }
            return;
        }
        match self.store.commit(&snap) {
            Ok(_) => self.last_persisted = snap,
            Err(error) => {
                self.store.mark_dirty();
                tracing::warn!(error = %error, "failed to persist session store");
            }
        }
    }

    /// Final persist (capturing live cwds), then freeze so the shutdown SIGHUPs'
    /// session removals don't clobber the file. Called once on the way out.
    pub fn freeze_and_persist(&mut self) {
        self.persist();
        self.persist_frozen = true;
    }

    /// Mark one loaded record as restored. Failed records remain in the next
    /// commit, while a live snapshot with the same name replaces its old data.
    pub fn mark_restored(&mut self, name: &str) {
        self.unrestored.retain(|state| state.name != name);
        self.persist();
    }

    pub fn get(&self, name: &str) -> Option<SessionHandle> {
        self.sessions.get(name).cloned()
    }

    pub fn by_identity(&self, identity: SessionIdentity) -> Option<SessionHandle> {
        self.sessions
            .values()
            .find(|h| {
                h.identity() == identity && h.meta.alive.load(std::sync::atomic::Ordering::Relaxed)
            })
            .cloned()
    }

    /// Called on the owning session thread with a freshly sampled foreground.
    pub fn report_agent(
        &mut self,
        identity: SessionIdentity,
        kind: asd_proto::AgentKind,
        action: &asd_proto::AgentHookAction,
        reference: &str,
        foreground: crate::agent_resume::AgentEvidence,
    ) -> Result<(), (u32, String)> {
        use crate::agent_resume::{AgentResumeRecord, validate_report};
        let invalid = |message: String| (code::INVALID_AGENT_REPORT, message);
        validate_report(kind, action, reference).map_err(invalid)?;
        let handle = self.by_identity(identity).ok_or_else(|| {
            (
                code::STALE_SESSION,
                "agent session identity is no longer live".into(),
            )
        })?;
        let current = self.agent_records.get(&identity);
        match action {
            asd_proto::AgentHookAction::Start { .. } => {
                if foreground.proven_kind(handle.spawn_command.as_deref()) != Some(kind) {
                    return Err(invalid(
                        "foreground or recorded launch does not prove the reported agent kind"
                            .into(),
                    ));
                }
                let name = handle.meta.name.lock().unwrap().clone();
                let states = self.persisted_state();
                if let Some(record) = states
                    .iter()
                    .filter_map(|state| state.agent_resume.as_ref())
                    .find(|record| record.kind == kind && record.session_ref == reference)
                    && crate::agent_resume::resume_owner(&states, record) != Some(name.as_str())
                {
                    return Err(invalid(
                        "agent reference is already owned by another persisted session".into(),
                    ));
                }
                if current.is_some_and(|r| r.kind == kind && r.session_ref == reference) {
                    return Ok(());
                }
                let reported_at_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX);
                self.commit_agent(
                    identity,
                    Some(AgentResumeRecord {
                        kind,
                        session_ref: reference.into(),
                        reported_at_ms,
                    }),
                )
            }
            asd_proto::AgentHookAction::End { .. } => {
                if !current.is_some_and(|r| r.kind == kind && r.session_ref == reference) {
                    return Err(invalid(
                        "agent end does not match the stored conversation".into(),
                    ));
                }
                self.commit_agent(identity, None)
            }
        }
    }

    pub fn clear_agent(&mut self, identity: SessionIdentity) -> Result<(), (u32, String)> {
        self.commit_agent(identity, None)
    }

    fn commit_agent(
        &mut self,
        identity: SessionIdentity,
        record: Option<crate::agent_resume::AgentResumeRecord>,
    ) -> Result<(), (u32, String)> {
        let handle = self.by_identity(identity).ok_or_else(|| {
            (
                code::STALE_SESSION,
                "agent session identity is no longer live".into(),
            )
        })?;
        if self.persist_frozen {
            return Err((
                code::PERSISTENCE_FAILURE,
                "session store is shutting down".into(),
            ));
        }
        let name = handle.meta.name.lock().unwrap().clone();
        let candidate: Vec<_> = self
            .persisted_state()
            .into_iter()
            .map(|state| {
                if state.name == name {
                    crate::store::SessionState {
                        agent_resume: record.clone(),
                        ..state
                    }
                } else {
                    state
                }
            })
            .collect();
        if let Err(error) = self.store.commit(&candidate) {
            // Replacement may have succeeded before directory sync failed.
            // Reconcile disk with the still-accepted Registry state on retry.
            self.store.mark_dirty();
            return Err((code::PERSISTENCE_FAILURE, error.to_string()));
        }
        match record {
            Some(record) => {
                self.agent_records.insert(identity, record);
            }
            None => {
                self.agent_records.remove(&identity);
            }
        }
        self.last_persisted = candidate;
        Ok(())
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let mut infos: Vec<_> = self.sessions.values().map(SessionHandle::info).collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    /// Callback at the session thread's endpoint: deregister and re-persist (so a
    /// killed or self-exited session drops off the list). A no-op on the file
    /// during shutdown, where `persist_frozen` is set.
    pub fn remove(&mut self, identity: SessionIdentity, exit: asd_proto::SessionExit) {
        let Some(name) = self
            .sessions
            .iter()
            .find_map(|(name, handle)| (handle.identity() == identity).then(|| name.clone()))
        else {
            return;
        };
        self.sessions.remove(&name);
        self.agent_records.remove(&identity);
        self.events
            .publish(crate::event_hub::CommittedSessionUpdate::Exited {
                identity,
                last_name: name,
                exit,
            });
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
        let info = handle.info();
        self.sessions.insert(new.to_string(), handle);
        self.events
            .publish(crate::event_hub::CommittedSessionUpdate::Renamed {
                old_name: old.into(),
                info,
            });
        self.persist();
        info!(from = %old, to = %new, "session renamed");
        Ok(())
    }

    pub fn kill(&self, name: &str, identity: SessionIdentity) -> Result<(), (u32, String)> {
        match self.sessions.get(name) {
            Some(h) => {
                if h.identity() != identity {
                    return Err((
                        code::STALE_SESSION,
                        format!("session '{name}' changed since it was selected"),
                    ));
                }
                let _ = h.tx.send(SessionMsg::Kill);
                Ok(())
            }
            None => Err((code::NO_SUCH_SESSION, format!("no such session '{name}'"))),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Shutdown (spec §5): ask each child to stop, wait 2s, then force any
    /// stragglers. Windows stops immediately because ConPTY has no deliverable
    /// graceful console signal. Blocking version, called only on daemon exit.
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
        info!(count = handles.len(), "shutting down sessions");
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
        info!("grace period over, force-killing remaining children");
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

#[cfg(test)]
mod identity_tests {
    use super::*;
    use crate::agent_resume::AgentEvidence::{Observed, Unavailable};

    fn test_registry() -> (
        Registry,
        std::sync::mpsc::Receiver<SessionMsg>,
        std::path::PathBuf,
    ) {
        let identity = SessionIdentity { instance_id: 7 };
        let (tx, rx) = std::sync::mpsc::channel();
        let meta = Arc::new(crate::session::SessionMeta {
            cols: std::sync::atomic::AtomicU16::new(80),
            rows: std::sync::atomic::AtomicU16::new(24),
            attached_clients: std::sync::atomic::AtomicU32::new(0),
            child_pid: std::sync::atomic::AtomicU32::new(42),
            alive: std::sync::atomic::AtomicBool::new(true),
            pty_master_fd: std::sync::atomic::AtomicI32::new(-1),
            title: Mutex::new(String::new()),
            status_line: Mutex::new(String::new()),
            state: Mutex::new(asd_proto::AgentState::Unknown),
            last_output_ms: std::sync::atomic::AtomicU64::new(100),
            name: Mutex::new("current".to_string()),
        });
        let handle = SessionHandle {
            name: "current".to_string(),
            identity,
            command: "sh".to_string(),
            spawn_command: None,
            created_ms: 100,
            tx,
            meta,
        };
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("asd-registry-test-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store =
            crate::store::SessionStore::open(dir.join("sessions.json"), dir.join("sessions.tsv"))
                .unwrap()
                .store;
        let mut registry = Registry::new(
            0,
            store,
            Vec::new(),
            std::env::temp_dir().join("unused-asd.sock"),
        )
        .unwrap();
        registry.sessions.insert("current".to_string(), handle);
        (registry, rx, dir)
    }

    #[test]
    fn rejected_agent_replacement_is_reconciled_by_sweep_or_shutdown() {
        use asd_proto::{AgentHookAction, AgentKind};
        for clear in [false, true] {
            for shutdown in [false, true] {
                let (mut registry, _rx, dir) = test_registry();
                let identity = SessionIdentity { instance_id: 7 };
                let start = AgentHookAction::Start {
                    source: "startup".into(),
                };
                registry
                    .report_agent(
                        identity,
                        AgentKind::Codex,
                        &start,
                        "accepted-a",
                        Observed(Some(AgentKind::Codex)),
                    )
                    .unwrap();
                let accepted = registry.snapshot()[0].agent_resume.clone();
                crate::store::FAIL_NEXT_PARENT_SYNC.with(|fail| fail.set(true));
                let result = if clear {
                    registry.clear_agent(identity)
                } else {
                    registry.report_agent(
                        identity,
                        AgentKind::Codex,
                        &start,
                        "rejected-b",
                        Observed(Some(AgentKind::Codex)),
                    )
                };
                let error = result.unwrap_err();
                assert_eq!(error.0, code::PERSISTENCE_FAILURE);
                assert!(error.1.contains("sync session-store directory"));
                assert_eq!(registry.snapshot()[0].agent_resume, accepted);
                let read_disk = || {
                    let document: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(dir.join("sessions.json")).unwrap())
                            .unwrap();
                    document["sessions"][0]["agent_resume"]["session_ref"].clone()
                };
                assert_eq!(
                    read_disk(),
                    if clear {
                        serde_json::Value::Null
                    } else {
                        serde_json::json!("rejected-b")
                    }
                );
                if shutdown {
                    registry.freeze_and_persist();
                } else {
                    registry.persist();
                }
                assert_eq!(
                    read_disk(),
                    "accepted-a",
                    "clear={clear}, shutdown={shutdown}"
                );
                assert_eq!(registry.snapshot()[0].agent_resume, accepted);
                std::fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[test]
    fn identity_and_agent_transactions_reject_stale_or_uncommitted_updates() {
        let (mut registry, rx, dir) = test_registry();
        let identity = SessionIdentity { instance_id: 7 };

        let error = registry
            .kill("current", SessionIdentity { instance_id: 8 })
            .unwrap_err();
        assert_eq!(error.0, code::STALE_SESSION);
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        registry.kill("current", identity).unwrap();
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::from_secs(1)),
            Ok(SessionMsg::Kill)
        ));
        use asd_proto::{AgentHookAction, AgentKind};
        let codex = Observed(Some(AgentKind::Codex));
        let start = AgentHookAction::Start {
            source: "startup".into(),
        };
        let end = AgentHookAction::End {
            reason: "other".into(),
        };
        assert_eq!(
            registry
                .report_agent(
                    SessionIdentity { instance_id: 8 },
                    AgentKind::Codex,
                    &start,
                    "one",
                    codex
                )
                .unwrap_err()
                .0,
            code::STALE_SESSION
        );
        assert!(
            registry
                .report_agent(
                    identity,
                    AgentKind::Codex,
                    &start,
                    "one",
                    Observed(Some(AgentKind::Claude))
                )
                .is_err()
        );
        assert!(
            registry
                .report_agent(identity, AgentKind::Codex, &start, "one", Unavailable)
                .is_err()
        );
        registry.sessions.get_mut("current").unwrap().spawn_command = Some("codex".into());
        registry
            .report_agent(identity, AgentKind::Codex, &start, "fallback", Unavailable)
            .unwrap();
        assert!(
            registry
                .report_agent(identity, AgentKind::Codex, &start, "wrong", Observed(None))
                .is_err()
        );
        registry
            .report_agent(identity, AgentKind::Codex, &start, "one", codex)
            .unwrap();
        let original = registry.snapshot()[0].agent_resume.clone().unwrap();
        registry.unrestored.push(crate::store::SessionState {
            name: "duplicate-loser".into(),
            cwd: None,
            command: None,
            agent_resume: Some(crate::agent_resume::AgentResumeRecord {
                reported_at_ms: original.reported_at_ms.saturating_sub(1),
                ..original.clone()
            }),
        });
        registry
            .report_agent(identity, AgentKind::Codex, &start, "one", codex)
            .unwrap();
        registry.unrestored.clear();
        registry
            .report_agent(
                identity,
                AgentKind::Codex,
                &AgentHookAction::Start {
                    source: "compact".into(),
                },
                "one",
                codex,
            )
            .unwrap();
        assert_eq!(registry.snapshot()[0].agent_resume, Some(original.clone()));
        assert!(
            registry
                .report_agent(identity, AgentKind::Claude, &end, "one", Unavailable)
                .is_err()
        );
        assert!(
            registry
                .report_agent(identity, AgentKind::Codex, &end, "old", Unavailable)
                .is_err()
        );
        assert_eq!(registry.snapshot()[0].agent_resume, Some(original.clone()));
        registry.unrestored.push(crate::store::SessionState {
            name: "retained".into(),
            cwd: None,
            command: None,
            agent_resume: Some(crate::agent_resume::AgentResumeRecord {
                session_ref: "owned".into(),
                ..original.clone()
            }),
        });
        assert!(
            registry
                .report_agent(identity, AgentKind::Codex, &start, "owned", codex)
                .is_err()
        );
        std::fs::remove_file(dir.join("sessions.json")).unwrap();
        std::fs::create_dir(dir.join("sessions.json")).unwrap();
        assert_eq!(
            registry
                .report_agent(identity, AgentKind::Codex, &start, "new", codex)
                .unwrap_err()
                .0,
            code::PERSISTENCE_FAILURE
        );
        assert!(registry.clear_agent(identity).is_err());
        assert_eq!(registry.snapshot()[0].agent_resume, Some(original));
        std::fs::remove_dir(dir.join("sessions.json")).unwrap();
        registry
            .report_agent(identity, AgentKind::Codex, &end, "one", Unavailable)
            .unwrap();
        assert!(registry.snapshot()[0].agent_resume.is_none());
        registry
            .report_agent(identity, AgentKind::Codex, &start, "new", codex)
            .unwrap();
        registry.clear_agent(identity).unwrap();
        assert!(registry.snapshot()[0].agent_resume.is_none());
        registry.rename("current", "renamed").unwrap();
        let exit = asd_proto::SessionExit {
            code: 0,
            signal: None,
        };
        registry.remove(SessionIdentity { instance_id: 8 }, exit.clone());
        assert_eq!(
            registry.list().len(),
            1,
            "an old exit cannot remove a replacement"
        );
        // Exit resolves the name only after taking the Registry lock.
        registry.remove(identity, exit);
        assert!(
            registry.list().is_empty(),
            "exit must resolve the identity under the Registry lock"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn killing_a_conpty_session_does_not_wait_for_a_console_signal() {
        crate::platform::harden_dll_search();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "asd-windows-kill-test-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pipe = std::path::PathBuf::from(format!(
            r"\\.\pipe\asd-windows-kill-test-{}-{unique}",
            std::process::id()
        ));
        let store =
            crate::store::SessionStore::open(dir.join("sessions.json"), dir.join("sessions.tsv"))
                .unwrap()
                .store;
        let registry = Arc::new(Mutex::new(
            Registry::new(0, store, Vec::new(), pipe).unwrap(),
        ));

        Registry::create(&registry, Some("doomed".to_string()), None, None).unwrap();
        let handle = registry.lock().unwrap().get("doomed").unwrap();
        let child_pid = handle
            .meta
            .child_pid
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_ne!(child_pid, 0, "the test must exercise a real ConPTY child");
        let started = std::time::Instant::now();
        handle.tx.send(SessionMsg::Kill).unwrap();
        let deadline = started + std::time::Duration::from_secs(1);
        while std::time::Instant::now() < deadline
            && registry.lock().unwrap().get("doomed").is_some()
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let closed = registry.lock().unwrap().get("doomed").is_none();
        let elapsed = started.elapsed();
        if !closed {
            crate::platform::kill_child(child_pid, true);
            let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while std::time::Instant::now() < cleanup_deadline
                && registry.lock().unwrap().get("doomed").is_some()
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        std::fs::remove_dir_all(&dir).ok();

        assert!(
            closed,
            "a Windows kill must close its ConPTY session within one second; elapsed {elapsed:?}"
        );
        assert!(!handle.meta.alive.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(
            handle
                .meta
                .child_pid
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }
}
