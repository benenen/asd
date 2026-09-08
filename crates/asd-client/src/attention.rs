//! Client-local attention, independent of transport and presentation effects.

use asd_proto::{
    AgentState, EventCursor, SessionEvent, SessionIdentity, SessionInfo, SessionUpdateCause,
};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionKind {
    Done,
    NeedsAttention,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionEffect {
    pub identity: SessionIdentity,
    pub name: String,
    pub kind: AttentionKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Attention {
    name: String,
    state: AgentState,
    worked_since_seen: bool,
    unread: Option<AttentionKind>,
    last_notified: Option<AttentionKind>,
    converged: bool,
}

impl Attention {
    fn seed(info: &SessionInfo) -> Self {
        Self {
            name: info.name.clone(),
            state: info.state,
            worked_since_seen: false,
            unread: None,
            last_notified: None,
            converged: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttentionTracker {
    cursor: Option<EventCursor>,
    sessions: HashMap<SessionIdentity, Attention>,
}

/// Lease gating never defers effects: gaining a lease cannot replay old alerts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttentionEndpoint {
    pub tracker: AttentionTracker,
    pub notification_lease: bool,
}

impl AttentionEndpoint {
    pub fn accept(
        &mut self,
        cursor: EventCursor,
        change: &crate::events::EventFeedChange,
    ) -> Vec<AttentionEffect> {
        use crate::events::EventFeedChange;
        let effects = match change {
            EventFeedChange::Reset {
                sessions,
                notification_lease,
            } => {
                self.notification_lease = *notification_lease;
                self.tracker.reset(cursor, sessions)
            }
            EventFeedChange::Changed { event, .. } => self.tracker.apply(cursor, event),
            EventFeedChange::NotificationLease(granted) => {
                self.notification_lease = *granted;
                Vec::new()
            }
        };
        if self.notification_lease {
            effects
        } else {
            Vec::new()
        }
    }
}

impl AttentionTracker {
    pub fn epoch(&self) -> Option<[u8; 16]> {
        self.cursor.map(|cursor| cursor.daemon_epoch)
    }

    /// Authoritative reset refreshes facts without inventing transitions.
    pub fn reset(&mut self, cursor: EventCursor, sessions: &[SessionInfo]) -> Vec<AttentionEffect> {
        if self
            .cursor
            .is_none_or(|old| old.daemon_epoch != cursor.daemon_epoch)
        {
            self.sessions.clear();
        }
        self.sessions
            .retain(|id, _| sessions.iter().any(|s| s.identity() == *id));
        for info in sessions {
            let entry = self
                .sessions
                .entry(info.identity())
                .or_insert_with(|| Attention::seed(info));
            entry.name = info.name.clone();
            entry.state = info.state;
        }
        self.cursor = Some(cursor);
        Vec::new()
    }

    /// Consume accepted feed events. Duplicate or foreign cursors are inert.
    pub fn apply(&mut self, cursor: EventCursor, event: &SessionEvent) -> Vec<AttentionEffect> {
        if self.cursor.is_none_or(|old| {
            old.daemon_epoch != cursor.daemon_epoch || old.sequence >= cursor.sequence
        }) {
            return Vec::new();
        }
        self.cursor = Some(cursor);
        match event {
            SessionEvent::Registered { info } => {
                self.sessions
                    .entry(info.identity())
                    .or_insert_with(|| Attention::seed(info));
            }
            SessionEvent::Renamed { info, .. } => {
                if let Some(entry) = self.sessions.get_mut(&info.identity()) {
                    entry.name = info.name.clone();
                }
            }
            SessionEvent::Exited { identity, .. } => {
                self.sessions.remove(identity);
            }
            SessionEvent::Updated {
                identity,
                patch,
                cause,
            } => {
                if let (Some(entry), Some(state)) = (self.sessions.get_mut(identity), patch.state) {
                    return update_attention(entry, *identity, state, *cause);
                }
            }
        }
        Vec::new()
    }

    pub fn view_converged(&mut self, identity: SessionIdentity) {
        if let Some(entry) = self.sessions.get_mut(&identity) {
            entry.converged = true;
            entry.unread = None;
            entry.last_notified = None;
            entry.worked_since_seen = false;
        }
    }

    pub fn view_left(&mut self, identity: SessionIdentity) {
        if let Some(entry) = self.sessions.get_mut(&identity) {
            if entry.converged && entry.state == AgentState::Working {
                entry.worked_since_seen = true;
            }
            entry.converged = false;
        }
    }

    pub fn unread(&self, identity: SessionIdentity) -> Option<AttentionKind> {
        self.sessions.get(&identity).and_then(|entry| entry.unread)
    }
}

fn update_attention(
    entry: &mut Attention,
    identity: SessionIdentity,
    state: AgentState,
    cause: SessionUpdateCause,
) -> Vec<AttentionEffect> {
    let previous = entry.state;
    entry.state = state;
    if cause == SessionUpdateCause::DetectorReload {
        if state == AgentState::Idle {
            entry.worked_since_seen = false;
        }
        return Vec::new();
    }
    if entry.converged {
        return Vec::new();
    }
    let kind = match state {
        AgentState::Working => {
            entry.worked_since_seen = true;
            if entry.unread == Some(AttentionKind::NeedsAttention) {
                entry.unread = None;
            }
            entry.last_notified = None;
            None
        }
        AgentState::Idle if entry.worked_since_seen => {
            entry.worked_since_seen = false;
            Some(AttentionKind::Done)
        }
        AgentState::Blocked if previous != AgentState::Blocked => {
            Some(AttentionKind::NeedsAttention)
        }
        _ => None,
    };
    let Some(kind) = kind else { return Vec::new() };
    entry.unread = Some(kind);
    if entry.last_notified == Some(kind) {
        return Vec::new();
    }
    entry.last_notified = Some(kind);
    vec![AttentionEffect {
        identity,
        name: entry.name.clone(),
        kind,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lease_grant_does_not_replay_unread_and_endpoints_are_independent() {
        use crate::events::EventFeedChange;
        let s = info(1, AgentState::Idle);
        let mut first = AttentionEndpoint::default();
        let second = AttentionEndpoint::default();
        first.accept(
            cursor(0),
            &EventFeedChange::Reset {
                sessions: vec![s.clone()],
                notification_lease: false,
            },
        );
        let event = change(&s, AgentState::Blocked, SessionUpdateCause::ScreenDetection);
        assert!(
            first
                .accept(
                    cursor(1),
                    &EventFeedChange::Changed {
                        sessions: vec![s.clone()],
                        event: Box::new(event)
                    }
                )
                .is_empty()
        );
        assert_eq!(
            first.tracker.unread(s.identity()),
            Some(AttentionKind::NeedsAttention)
        );
        assert_eq!(second.tracker.unread(s.identity()), None);
        assert!(
            first
                .accept(cursor(1), &EventFeedChange::NotificationLease(true))
                .is_empty()
        );
        assert!(
            first
                .accept(cursor(1), &EventFeedChange::NotificationLease(true))
                .is_empty()
        );
    }
    use asd_proto::{
        AgentState, EventCursor, SessionEvent, SessionInfo, SessionUpdateCause, SessionUpdatePatch,
    };

    fn cursor(sequence: u64) -> EventCursor {
        EventCursor {
            daemon_epoch: [7; 16],
            sequence,
        }
    }
    fn info(id: u128, state: AgentState) -> SessionInfo {
        SessionInfo {
            name: "agent".into(),
            instance_id: id,
            command: "sh".into(),
            title: String::new(),
            status_line: String::new(),
            task: None,
            created_ms: 0,
            idle_ms: 0,
            running: false,
            state,
            attached_clients: 0,
            pid: 1,
            cols: 80,
            rows: 24,
        }
    }
    fn change(s: &SessionInfo, state: AgentState, cause: SessionUpdateCause) -> SessionEvent {
        SessionEvent::Updated {
            identity: s.identity(),
            cause,
            patch: SessionUpdatePatch {
                state: Some(state),
                command: None,
                title: None,
                status_line: None,
                task: None,
                idle_ms: None,
                running: None,
                attached_clients: None,
                pid: None,
                cols: None,
                rows: None,
            },
        }
    }
    fn update(
        t: &mut AttentionTracker,
        s: &SessionInfo,
        seq: u64,
        state: AgentState,
    ) -> Vec<AttentionEffect> {
        t.apply(
            cursor(seq),
            &change(s, state, SessionUpdateCause::ScreenDetection),
        )
    }
    #[test]
    fn seed_unknown_work_idle_and_duplicate() {
        let s = info(1, AgentState::Working);
        let mut t = AttentionTracker::default();
        assert!(t.reset(cursor(0), std::slice::from_ref(&s)).is_empty());
        assert!(update(&mut t, &s, 1, AgentState::Idle).is_empty());
        assert!(update(&mut t, &s, 2, AgentState::Working).is_empty());
        assert!(update(&mut t, &s, 3, AgentState::Unknown).is_empty());
        assert_eq!(
            update(&mut t, &s, 4, AgentState::Idle)[0].kind,
            AttentionKind::Done
        );
        assert!(update(&mut t, &s, 4, AgentState::Idle).is_empty());
        assert!(update(&mut t, &s, 5, AgentState::Idle).is_empty());
        assert_eq!(t.unread(s.identity()), Some(AttentionKind::Done));
    }
    #[test]
    fn blocked_is_once_until_work_resumes() {
        let s = info(1, AgentState::Idle);
        let mut t = AttentionTracker::default();
        t.reset(cursor(0), std::slice::from_ref(&s));
        assert_eq!(
            update(&mut t, &s, 1, AgentState::Blocked)[0].kind,
            AttentionKind::NeedsAttention
        );
        assert!(update(&mut t, &s, 2, AgentState::Blocked).is_empty());
        update(&mut t, &s, 3, AgentState::Working);
        assert_eq!(t.unread(s.identity()), None);
        assert_eq!(update(&mut t, &s, 4, AgentState::Blocked).len(), 1);
    }
    #[test]
    fn exact_convergence_and_leaving_working_rearms() {
        let s = info(1, AgentState::Idle);
        let other = info(2, AgentState::Idle);
        let mut t = AttentionTracker::default();
        t.reset(cursor(0), std::slice::from_ref(&s));
        update(&mut t, &s, 1, AgentState::Working);
        t.view_converged(other.identity());
        assert_eq!(update(&mut t, &s, 2, AgentState::Idle).len(), 1);
        t.view_converged(s.identity());
        assert_eq!(t.unread(s.identity()), None);
        assert!(update(&mut t, &s, 3, AgentState::Blocked).is_empty());
        update(&mut t, &s, 4, AgentState::Working);
        t.view_left(s.identity());
        assert_eq!(update(&mut t, &s, 5, AgentState::Idle).len(), 1);
    }
    #[test]
    fn reset_preserves_survivors_but_epoch_and_replacement_clear() {
        let s = info(1, AgentState::Idle);
        let mut t = AttentionTracker::default();
        t.reset(cursor(0), std::slice::from_ref(&s));
        update(&mut t, &s, 1, AgentState::Blocked);
        assert!(t.reset(cursor(5), std::slice::from_ref(&s)).is_empty());
        assert_eq!(t.unread(s.identity()), Some(AttentionKind::NeedsAttention));
        let next = info(2, AgentState::Working);
        t.reset(cursor(6), std::slice::from_ref(&next));
        assert_eq!(t.unread(s.identity()), None);
        assert!(update(&mut t, &next, 7, AgentState::Idle).is_empty());
        update(&mut t, &next, 8, AgentState::Blocked);
        t.reset(
            EventCursor {
                daemon_epoch: [8; 16],
                sequence: 0,
            },
            std::slice::from_ref(&next),
        );
        assert_eq!(t.unread(next.identity()), None);
    }
    #[test]
    fn reload_does_not_arm_or_complete_work() {
        let s = info(1, AgentState::Idle);
        let mut t = AttentionTracker::default();
        t.reset(cursor(0), std::slice::from_ref(&s));
        t.apply(
            cursor(1),
            &change(&s, AgentState::Working, SessionUpdateCause::DetectorReload),
        );
        assert!(update(&mut t, &s, 2, AgentState::Idle).is_empty());
        update(&mut t, &s, 3, AgentState::Working);
        assert!(
            t.apply(
                cursor(4),
                &change(&s, AgentState::Idle, SessionUpdateCause::DetectorReload)
            )
            .is_empty()
        );
        assert!(update(&mut t, &s, 5, AgentState::Idle).is_empty());
    }
    #[test]
    fn rename_retains_attention_and_effect_uses_new_name() {
        let s = info(1, AgentState::Idle);
        let mut t = AttentionTracker::default();
        t.reset(cursor(0), std::slice::from_ref(&s));
        update(&mut t, &s, 1, AgentState::Working);
        let mut renamed = s.clone();
        renamed.name = "renamed".into();
        t.apply(
            cursor(2),
            &SessionEvent::Renamed {
                old_name: s.name.clone(),
                info: renamed,
            },
        );
        assert_eq!(update(&mut t, &s, 3, AgentState::Idle)[0].name, "renamed");
    }
}
