//! Durable task metadata changes, serialized with registry persistence.
use super::Registry;
use asd_proto::{SessionIdentity, SessionTask, SessionUpdateCause, code};

impl Registry {
    pub fn set_task(
        &mut self,
        identity: SessionIdentity,
        task: Option<SessionTask>,
    ) -> Result<(), (u32, String)> {
        let handle = self.by_identity(identity).ok_or_else(|| {
            (
                code::STALE_SESSION,
                "session identity is no longer live".into(),
            )
        })?;
        if self.persist_frozen {
            return Err((
                code::PERSISTENCE_FAILURE,
                "session store is shutting down".into(),
            ));
        }
        let task = task
            .map(|task| {
                task.validate()?;
                let directory = std::path::Path::new(&task.directory);
                if !directory.is_absolute() {
                    return Err("task directory must be an absolute path on the daemon host".into());
                }
                let directory = directory
                    .canonicalize()
                    .map_err(|e| format!("cannot resolve task directory: {e}"))?;
                if !directory.is_dir() {
                    return Err("task directory is not a directory".into());
                }
                let directory = directory
                    .to_str()
                    .ok_or("task directory is not valid UTF-8")?
                    .to_owned();
                let task = SessionTask { directory, ..task };
                task.validate()?;
                Ok(task)
            })
            .transpose()
            .map_err(|message| (code::INVALID_TASK, message))?;
        if *handle.meta.task.lock().unwrap() == task {
            return Ok(());
        }
        let name = handle.meta.name.lock().unwrap().clone();
        let candidate: Vec<_> = self
            .persisted_state()
            .into_iter()
            .map(|state| {
                if state.name == name {
                    crate::store::SessionState {
                        task: task.clone(),
                        ..state
                    }
                } else {
                    state
                }
            })
            .collect();
        if let Err(error) = self.store.commit(&candidate) {
            self.store.mark_dirty();
            return Err((code::PERSISTENCE_FAILURE, error.to_string()));
        }
        *handle.meta.task.lock().unwrap() = task.clone();
        self.last_persisted = candidate;
        let mut patch = crate::event_hub::empty_patch();
        patch.task = Some(task);
        self.events
            .publish(crate::event_hub::CommittedSessionUpdate::Updated {
                identity,
                patch,
                cause: SessionUpdateCause::TaskChanged,
            });
        Ok(())
    }

    /// Resolve only on the daemon: a remote client's PID namespace is unrelated.
    pub fn review_target(
        &self,
        identity: SessionIdentity,
    ) -> Result<(Option<SessionTask>, std::path::PathBuf), (u32, String)> {
        let handle = self.by_identity(identity).ok_or_else(|| {
            (
                code::STALE_SESSION,
                "session identity is no longer live".into(),
            )
        })?;
        let task = handle.meta.task.lock().unwrap().clone();
        let directory = task
            .as_ref()
            .map(|task| std::path::PathBuf::from(&task.directory))
            .or_else(|| {
                crate::store::read_cwd(
                    handle
                        .meta
                        .child_pid
                        .load(std::sync::atomic::Ordering::Relaxed),
                )
            })
            .ok_or_else(|| {
                (
                    code::REVIEW_FAILED,
                    "cannot determine the session directory on the daemon host".into(),
                )
            })?;
        Ok((task, directory))
    }
}
