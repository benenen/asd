//! Validated, identity-keyed session events shared by CLI and UI consumers.
use asd_proto::{
    EventCursor, Frame, SessionEvent, SessionIdentity, SessionInfo, SessionUpdatePatch,
};
use std::collections::HashMap;

#[derive(Default)]
pub struct EventFeed {
    cursor: Option<EventCursor>,
    sessions: HashMap<SessionIdentity, SessionInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventFeedChange {
    Reset {
        sessions: Vec<SessionInfo>,
        notification_lease: bool,
    },
    Changed {
        sessions: Vec<SessionInfo>,
        event: Box<SessionEvent>,
    },
    NotificationLease(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventOrderError;
impl std::fmt::Display for EventOrderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid event stream ordering; reconnect with a reset")
    }
}
impl std::error::Error for EventOrderError {}

impl EventFeed {
    /// A valid replay start retains the projection and only refreshes the lease.
    pub fn start(&mut self, frame: Frame) -> Result<EventFeedChange, EventOrderError> {
        let Frame::EventStreamStarted {
            cursor,
            sessions,
            reset,
            notification_lease,
        } = frame
        else {
            return Err(EventOrderError);
        };
        if !reset {
            if self.cursor != Some(cursor) || !sessions.is_empty() {
                return Err(EventOrderError);
            }
            return Ok(EventFeedChange::NotificationLease(notification_lease));
        }
        let projection: HashMap<_, _> =
            sessions.iter().map(|s| (s.identity(), s.clone())).collect();
        if projection.len() != sessions.len() {
            return Err(EventOrderError);
        }
        self.sessions = projection;
        self.cursor = Some(cursor);
        Ok(EventFeedChange::Reset {
            sessions: self.sessions(),
            notification_lease,
        })
    }

    /// Validate the entire event before changing either cursor or projection.
    /// Consumers read `last_cursor` immediately after this result for effects.
    pub fn apply(&mut self, frame: Frame) -> Result<EventFeedChange, EventOrderError> {
        let previous = self.cursor.ok_or(EventOrderError)?;
        if let Frame::NotificationLeaseChanged { granted } = frame {
            return Ok(EventFeedChange::NotificationLease(granted));
        }
        let Frame::SessionEvent { cursor, event } = frame else {
            return Err(EventOrderError);
        };
        if cursor.daemon_epoch != previous.daemon_epoch
            || previous.sequence.checked_add(1) != Some(cursor.sequence)
        {
            return Err(EventOrderError);
        }
        match &event {
            SessionEvent::Registered { info } => {
                if self.sessions.contains_key(&info.identity()) {
                    return Err(EventOrderError);
                }
                self.sessions.insert(info.identity(), info.clone());
            }
            SessionEvent::Updated {
                identity, patch, ..
            } => {
                let info = self.sessions.get_mut(identity).ok_or(EventOrderError)?;
                apply_patch(info, patch);
            }
            SessionEvent::Renamed { info, .. } => {
                if !self.sessions.contains_key(&info.identity()) {
                    return Err(EventOrderError);
                }
                self.sessions.insert(info.identity(), info.clone());
            }
            SessionEvent::Exited { identity, .. } => {
                if self.sessions.remove(identity).is_none() {
                    return Err(EventOrderError);
                }
            }
        }
        self.cursor = Some(cursor);
        Ok(EventFeedChange::Changed {
            sessions: self.sessions(),
            event: Box::new(event),
        })
    }

    pub fn last_cursor(&self) -> Option<EventCursor> {
        self.cursor
    }

    pub fn sessions(&self) -> Vec<SessionInfo> {
        let mut sessions: Vec<_> = self.sessions.values().cloned().collect();
        sessions.sort_by(|a, b| a.name.cmp(&b.name).then(a.instance_id.cmp(&b.instance_id)));
        sessions
    }
}

fn apply_patch(info: &mut SessionInfo, patch: &SessionUpdatePatch) {
    macro_rules! owned { ($($field:ident),*) => { $(if let Some(value) = &patch.$field { info.$field = value.clone(); })* }; }
    macro_rules! copied { ($($field:ident),*) => { $(if let Some(value) = patch.$field { info.$field = value; })* }; }
    owned!(command, title, status_line, task);
    copied!(idle_ms, running, state, attached_clients, pid, cols, rows);
}

#[cfg(test)]
mod tests {
    use super::*;
    use asd_proto::{
        AgentState, EventCursor, Frame, SessionEvent, SessionInfo, SessionUpdateCause,
        SessionUpdatePatch,
    };

    fn info(id: u128, name: &str) -> SessionInfo {
        SessionInfo {
            name: name.into(),
            instance_id: id,
            command: "sh".into(),
            title: String::new(),
            status_line: String::new(),
            task: None,
            created_ms: 0,
            idle_ms: 0,
            running: true,
            state: AgentState::Unknown,
            attached_clients: 0,
            pid: 1,
            cols: 80,
            rows: 24,
        }
    }
    fn cursor(sequence: u64) -> EventCursor {
        EventCursor {
            daemon_epoch: [7; 16],
            sequence,
        }
    }
    fn start(sequence: u64, reset: bool, sessions: Vec<SessionInfo>) -> Frame {
        Frame::EventStreamStarted {
            cursor: cursor(sequence),
            reset,
            sessions,
            notification_lease: false,
        }
    }
    #[test]
    fn task_patch_distinguishes_unchanged_set_and_clear() {
        let mut info = info(1, "work");
        let task = asd_proto::SessionTask {
            description: "Review changes".into(),
            directory: "/srv/worktree".into(),
        };
        let mut patch = SessionUpdatePatch {
            command: None,
            title: None,
            status_line: None,
            task: Some(Some(task.clone())),
            idle_ms: None,
            running: None,
            state: None,
            attached_clients: None,
            pid: None,
            cols: None,
            rows: None,
        };
        apply_patch(&mut info, &patch);
        assert_eq!(info.task, Some(task.clone()));
        patch.task = None;
        patch.running = Some(false);
        apply_patch(&mut info, &patch);
        assert_eq!(info.task, Some(task));
        patch.task = Some(None);
        apply_patch(&mut info, &patch);
        assert_eq!(info.task, None);
    }

    #[test]
    fn rejects_bad_cursor_before_mutating_projection() {
        let mut feed = EventFeed::default();
        feed.start(start(3, true, vec![info(1, "original")]))
            .unwrap();
        for c in [
            cursor(3),
            cursor(2),
            cursor(5),
            EventCursor {
                daemon_epoch: [9; 16],
                sequence: 4,
            },
        ] {
            assert!(
                feed.apply(Frame::SessionEvent {
                    cursor: c,
                    event: SessionEvent::Registered {
                        info: info(2, "bad")
                    }
                })
                .is_err()
            );
            assert_eq!(feed.last_cursor(), Some(cursor(3)));
            assert_eq!(feed.sessions(), vec![info(1, "original")]);
        }
    }
    #[test]
    fn replay_preserves_projection_and_identity_survives_name_free_patch() {
        let mut feed = EventFeed::default();
        feed.start(start(1, true, vec![info(1, "z")])).unwrap();
        assert!(matches!(
            feed.start(start(1, false, vec![])).unwrap(),
            EventFeedChange::NotificationLease(false)
        ));
        feed.apply(Frame::SessionEvent {
            cursor: cursor(2),
            event: SessionEvent::Renamed {
                old_name: "z".into(),
                info: info(1, "b"),
            },
        })
        .unwrap();
        feed.apply(Frame::SessionEvent {
            cursor: cursor(3),
            event: SessionEvent::Registered { info: info(2, "a") },
        })
        .unwrap();
        let patch = SessionUpdatePatch {
            command: None,
            title: Some("title".into()),
            status_line: None,
            task: None,
            idle_ms: None,
            running: None,
            state: None,
            attached_clients: None,
            pid: None,
            cols: None,
            rows: None,
        };
        feed.apply(Frame::SessionEvent {
            cursor: cursor(4),
            event: SessionEvent::Updated {
                identity: info(1, "z").identity(),
                patch,
                cause: SessionUpdateCause::ForegroundChanged,
            },
        })
        .unwrap();
        assert_eq!(
            feed.sessions()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(feed.sessions()[1].title, "title");
        feed.apply(Frame::SessionEvent {
            cursor: cursor(5),
            event: SessionEvent::Exited {
                identity: info(1, "z").identity(),
                last_name: "b".into(),
                exit: asd_proto::SessionExit {
                    code: 0,
                    signal: None,
                },
            },
        })
        .unwrap();
        assert_eq!(feed.sessions(), vec![info(2, "a")]);
    }
    #[test]
    fn replay_requires_matching_existing_cursor_and_reset_reseeds() {
        let mut feed = EventFeed::default();
        assert!(feed.start(start(0, false, vec![])).is_err());
        feed.start(start(2, true, vec![info(1, "a")])).unwrap();
        assert!(feed.start(start(1, false, vec![])).is_err());
        feed.start(start(8, true, vec![info(2, "b")])).unwrap();
        assert_eq!(feed.sessions(), vec![info(2, "b")]);
        assert_eq!(feed.last_cursor(), Some(cursor(8)));
    }
}
