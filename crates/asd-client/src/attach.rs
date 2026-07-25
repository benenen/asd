//! Attach convergence state machine shared by every long-lived asd client
//! (TUI and GUI). While a session switch is in flight the connection may
//! receive frames belonging to the old session — this state machine tracks
//! the `pending` Snapshot count and the `showing` name, so each arriving
//! frame is routed (or dropped) correctly.
//!
//! The rules, which every asd client must observe:
//!
//! - An Output arriving while a switch is in flight (`pending > 0`) belongs
//!   to the session we just left → drop it.
//! - A Snapshot arriving while `pending > 1` belongs to a superseded attach
//!   (the user switched again before the first reply landed) → drop it.
//! - A `SESSION_EXITED` error pins the name of the ended session only when
//!   no switch is in flight; with a pending attach it belongs to the session
//!   we just left and must not take the current view's name.
//! - A failed Attach (`NO_SUCH_SESSION` while pending) drains one pending
//!   count so later Snapshots stay aligned.
//! - When the viewed session is renamed, the state machine re-tags its
//!   `showing` name so subsequent frames continue to match.

/// Attach bookkeeping for exactly one connection.
///
/// `pending` counts Attach frames whose Snapshot has not arrived yet: while
/// `> 0` a live Output is stale (belongs to a session we just left), and while
/// `> 1` an arriving Snapshot belongs to a superseded attach (a quick switch).
/// `showing` names the session the forwarded frames are tagged with — the
/// current view; the UI drops frames tagged with anything else.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Attach {
    pending: usize,
    showing: Option<String>,
}

impl Attach {
    /// Begin an Attach to `name`. Returns whether the connection was already
    /// attached (so the caller writes a `Detach` first — switching sessions on
    /// one connection is detach-then-attach).
    pub fn begin(&mut self, name: String) -> bool {
        let was_attached = self.showing.is_some();
        self.pending += 1;
        self.showing = Some(name);
        was_attached
    }

    /// A Snapshot arrived: the session name to tag it with, or `None` when it
    /// belongs to a superseded attach and must be dropped.
    pub fn on_snapshot(&mut self) -> Option<String> {
        if self.pending > 1 {
            self.pending -= 1; // superseded attach — not our view
            return None;
        }
        self.pending = 0;
        self.showing.clone()
    }

    /// An Output arrived: the session name to tag it with, or `None` while a
    /// switch is still converging (the bytes belong to a session we just left).
    pub fn on_output(&self) -> Option<String> {
        if self.pending > 0 {
            return None;
        }
        self.showing.clone()
    }

    /// The attached session exited (`SESSION_EXITED` carries no name). Returns
    /// the ended session's name only when it can be pinned on the current view —
    /// with no switch in flight; with one pending, the exit belongs to the
    /// session we just left and taking `showing` would drop the incoming
    /// Snapshot of the new one.
    pub fn on_session_exited(&mut self) -> Option<String> {
        if self.pending == 0 {
            self.showing.take()
        } else {
            None
        }
    }

    /// The client renamed a session. If it is the one being shown, re-tag the
    /// view: the UI optimistically renames at the same time, so frames still
    /// tagged with the old name would otherwise be dropped as a mismatch.
    pub fn on_rename(&mut self, old: &str, new: &str) {
        if self.showing.as_deref() == Some(old) {
            self.showing = Some(new.to_string());
        }
    }

    /// A pending Attach failed (`NO_SUCH_SESSION`: the session died before we
    /// attached). Drains one pending count; returns the ended name only if that
    /// was the newest attach, so the client stops holding the pane for a
    /// Snapshot that will never come. Caller guards `pending > 0`.
    pub fn on_attach_failed(&mut self) -> Option<String> {
        self.pending -= 1;
        if self.pending == 0 {
            self.showing.take()
        } else {
            None
        }
    }

    /// Whether the connection is currently attached (showing some session).
    pub fn is_attached(&self) -> bool {
        self.showing.is_some()
    }

    /// How many Attach frames are still waiting for their Snapshot.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// The name of the currently shown session, if any.
    pub fn showing(&self) -> Option<&str> {
        self.showing.as_deref()
    }

    /// Forcibly detach (e.g. the user issued an explicit Detach command).
    /// Returns the name that was being shown.
    pub fn detach(&mut self) -> Option<String> {
        // pending stays: any Snapshot still in flight must drain through
        // on_snapshot (showing is None, nothing is forwarded) so the count
        // stays aligned.
        self.showing.take()
    }
}

#[cfg(test)]
mod tests {
    use super::Attach;

    fn s(name: &str) -> Option<String> {
        Some(name.to_string())
    }

    #[test]
    fn first_attach_needs_no_detach_switch_does() {
        let mut at = Attach::default();
        assert!(!at.begin("a".into()));
        at.on_snapshot(); // converges
        assert!(at.begin("b".into()));
    }

    #[test]
    fn snapshot_then_output_tag_the_current_view() {
        let mut at = Attach::default();
        at.begin("a".into());
        assert_eq!(at.on_snapshot(), s("a"));
        assert_eq!(at.on_output(), s("a"));
    }

    #[test]
    fn output_is_dropped_until_the_snapshot_converges() {
        let mut at = Attach::default();
        at.begin("a".into());
        assert_eq!(at.on_output(), None);
        assert_eq!(at.on_snapshot(), s("a"));
        assert_eq!(at.on_output(), s("a"));
    }

    #[test]
    fn quick_switch_drops_the_superseded_snapshot() {
        let mut at = Attach::default();
        at.begin("a".into());
        at.begin("b".into());
        assert_eq!(at.on_snapshot(), None); // a's snapshot — superseded
        assert_eq!(at.on_snapshot(), s("b"));
        assert_eq!(at.on_output(), s("b"));
    }

    #[test]
    fn session_exit_pins_the_name_only_when_settled() {
        let mut at = Attach::default();
        at.begin("a".into());
        at.on_snapshot();
        assert_eq!(at.on_session_exited(), s("a"));
        assert_eq!(at.on_output(), None);

        // Switch in flight: exit belongs to the session we left.
        let mut at = Attach::default();
        at.begin("a".into());
        at.on_snapshot();
        at.begin("b".into());
        assert_eq!(at.on_session_exited(), None);
        assert_eq!(at.on_snapshot(), s("b"));
    }

    #[test]
    fn failed_attach_drains_and_reports_only_the_newest() {
        let mut at = Attach::default();
        at.begin("gone".into());
        assert_eq!(at.on_attach_failed(), s("gone"));

        let mut at = Attach::default();
        at.begin("a".into());
        at.begin("b".into());
        assert_eq!(at.on_attach_failed(), None); // a failed, b still pending
        assert_eq!(at.on_snapshot(), s("b"));
    }

    #[test]
    fn rename_of_the_shown_session_retags_the_view() {
        let mut at = Attach::default();
        at.begin("s0".into());
        assert_eq!(at.on_snapshot(), s("s0"));
        at.on_rename("s0", "zzz");
        assert_eq!(at.on_output(), s("zzz"));

        // Renaming a different session leaves the view alone.
        let mut at = Attach::default();
        at.begin("a".into());
        at.on_snapshot();
        at.on_rename("other", "new");
        assert_eq!(at.on_output(), s("a"));
    }

    #[test]
    fn reattach_after_kill_routes_the_new_sessions_snapshot() {
        let mut at = Attach::default();
        at.begin("a".into());
        assert_eq!(at.on_snapshot(), s("a"));
        assert_eq!(at.on_session_exited(), s("a"));
        assert_eq!(at.showing, None);
        assert!(!at.begin("b".into()));
        assert_eq!(at.on_snapshot(), s("b"));
        assert_eq!(at.on_output(), s("b"));
    }

    #[test]
    fn explicit_detach_drains_pending_snapshots() {
        let mut at = Attach::default();
        at.begin("a".into());
        assert_eq!(at.detach(), s("a"));
        // Snapshot still in flight drains harmlessly — showing is None.
        assert_eq!(at.on_snapshot(), None);
    }
}
