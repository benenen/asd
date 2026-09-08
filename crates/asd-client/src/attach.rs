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
    identity: Option<asd_proto::SessionIdentity>,
    view_id: u64,
}

/// The exact session instance whose frames have converged on a client view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedSession {
    pub name: String,
    pub identity: asd_proto::SessionIdentity,
}

impl Attach {
    /// Begin an Attach to `name`. Returns whether the connection was already
    /// attached (so the caller writes a `Detach` first — switching sessions on
    /// one connection is detach-then-attach).
    pub fn begin(&mut self, name: String) -> bool {
        self.begin_view(name, 0)
    }

    /// Begin a view with an opaque client-generated identity. Ratatui uses a
    /// fresh nonzero id for every attach; shared GUI clients use [`begin`].
    pub fn begin_view(&mut self, name: String, view_id: u64) -> bool {
        let was_attached = self.showing.is_some();
        self.pending += 1;
        self.showing = Some(name);
        self.identity = None;
        self.view_id = view_id;
        was_attached
    }

    /// A Snapshot arrived: its converged session identity and name, or `None`
    /// when it belongs to a superseded attach and must be dropped.
    pub fn on_snapshot(&mut self, identity: asd_proto::SessionIdentity) -> Option<AttachedSession> {
        if self.pending > 1 {
            self.pending -= 1; // superseded attach — not our view
            return None;
        }
        self.pending = 0;
        let name = self.showing.clone()?;
        self.identity = Some(identity);
        Some(AttachedSession { name, identity })
    }

    /// An Output arrived: the converged session identity and name to tag it
    /// with, or `None` while a switch is still converging (the bytes belong to
    /// the session we just left).
    pub fn on_output(&self) -> Option<AttachedSession> {
        if self.pending > 0 {
            return None;
        }
        Some(AttachedSession {
            name: self.showing.clone()?,
            identity: self.identity?,
        })
    }

    /// The attached session exited (`SESSION_EXITED` carries no name). Returns
    /// the ended session's name only when it can be pinned on the current view —
    /// with no switch in flight; with one pending, the exit belongs to the
    /// session we just left and taking `showing` would drop the incoming
    /// Snapshot of the new one.
    pub fn on_session_exited(&mut self) -> Option<String> {
        if self.pending == 0 {
            self.view_id = 0;
            self.identity = None;
            self.showing.take()
        } else {
            None
        }
    }

    /// Another TUI took this exact view attempt. The opaque id, rather than the
    /// mutable session name, prevents delayed revocations from clearing a later
    /// attach and remains valid across external renames.
    pub fn on_view_revoked(&mut self, view_id: u64) -> Option<String> {
        if self.pending == 0 && self.view_id == view_id {
            self.view_id = 0;
            self.identity = None;
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

    /// Apply a daemon rename only when it belongs to this exact TUI view.
    pub fn on_view_renamed(&mut self, view_id: u64, old: &str, new: &str) -> bool {
        if self.view_id != view_id || self.showing.as_deref() != Some(old) {
            return false;
        }
        self.showing = Some(new.to_string());
        true
    }

    /// A pending Attach failed (`NO_SUCH_SESSION`: the session died before we
    /// attached). Drains one pending count; returns the ended name only if that
    /// was the newest attach, so the client stops holding the pane for a
    /// Snapshot that will never come. Caller guards `pending > 0`.
    pub fn on_attach_failed(&mut self) -> Option<String> {
        self.pending -= 1;
        if self.pending == 0 {
            self.view_id = 0;
            self.identity = None;
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
        self.view_id = 0;
        self.identity = None;
        self.showing.take()
    }
}

#[cfg(test)]
mod tests {
    use super::Attach;
    use asd_proto::SessionIdentity;

    fn id(instance_id: u128) -> SessionIdentity {
        SessionIdentity { instance_id }
    }

    fn tagged(value: Option<super::AttachedSession>) -> Option<(String, SessionIdentity)> {
        value.map(|attached| (attached.name, attached.identity))
    }

    #[test]
    fn first_attach_needs_no_detach_switch_does() {
        let mut at = Attach::default();
        assert!(!at.begin("a".into()));
        at.on_snapshot(id(1)); // converges
        assert!(at.begin("b".into()));
    }

    #[test]
    fn snapshot_then_output_tag_the_current_view() {
        let mut at = Attach::default();
        at.begin("a".into());
        assert_eq!(tagged(at.on_snapshot(id(1))), Some(("a".into(), id(1))));
        assert_eq!(tagged(at.on_output()), Some(("a".into(), id(1))));
    }

    #[test]
    fn output_is_dropped_until_the_snapshot_converges() {
        let mut at = Attach::default();
        at.begin("a".into());
        assert_eq!(at.on_output(), None);
        assert_eq!(tagged(at.on_snapshot(id(1))), Some(("a".into(), id(1))));
        assert_eq!(tagged(at.on_output()), Some(("a".into(), id(1))));
    }

    #[test]
    fn quick_switch_drops_the_superseded_snapshot() {
        let mut at = Attach::default();
        at.begin("a".into());
        at.begin("b".into());
        assert_eq!(at.on_snapshot(id(1)), None); // a's snapshot — superseded
        assert_eq!(tagged(at.on_snapshot(id(2))), Some(("b".into(), id(2))));
        assert_eq!(tagged(at.on_output()), Some(("b".into(), id(2))));
    }

    #[test]
    fn session_exit_pins_the_name_only_when_settled() {
        let mut at = Attach::default();
        at.begin("a".into());
        at.on_snapshot(id(1));
        assert_eq!(at.on_session_exited(), Some("a".into()));
        assert_eq!(at.on_output(), None);

        // Switch in flight: exit belongs to the session we left.
        let mut at = Attach::default();
        at.begin("a".into());
        at.on_snapshot(id(1));
        at.begin("b".into());
        assert_eq!(at.on_session_exited(), None);
        assert_eq!(tagged(at.on_snapshot(id(2))), Some(("b".into(), id(2))));
    }

    #[test]
    fn failed_attach_drains_and_reports_only_the_newest() {
        let mut at = Attach::default();
        at.begin("gone".into());
        assert_eq!(at.on_attach_failed(), Some("gone".into()));

        let mut at = Attach::default();
        at.begin("a".into());
        at.begin("b".into());
        assert_eq!(at.on_attach_failed(), None); // a failed, b still pending
        assert_eq!(tagged(at.on_snapshot(id(2))), Some(("b".into(), id(2))));
    }

    /// Bug regression: renaming the session being viewed re-tags the view, so
    /// its Output keeps matching the client's optimistically-renamed active
    /// session. Before the fix every frame after a rename-while-viewing was
    /// dropped — the pane froze (input still executed) until a switch away and
    /// back. The common trigger: create a session, rename it, start typing.
    #[test]
    fn rename_of_the_shown_session_retags_the_view() {
        let mut at = Attach::default();
        at.begin("s0".into());
        assert_eq!(tagged(at.on_snapshot(id(1))), Some(("s0".into(), id(1))));
        at.on_rename("s0", "zzz");
        assert_eq!(tagged(at.on_output()), Some(("zzz".into(), id(1))));

        // Renaming a different session leaves the view alone.
        let mut at = Attach::default();
        at.begin("a".into());
        at.on_snapshot(id(1));
        at.on_rename("other", "new");
        assert_eq!(tagged(at.on_output()), Some(("a".into(), id(1))));
    }

    /// Bug regression (client side): after the attached session is killed and
    /// exits, attaching a brand-new session routes that session's Snapshot to
    /// the pane — the frame is NOT filtered out. (The daemon-side fix is what
    /// makes the Snapshot actually arrive; this pins the client's routing.)
    #[test]
    fn reattach_after_kill_routes_the_new_sessions_snapshot() {
        let mut at = Attach::default();
        at.begin("a".into());
        assert_eq!(tagged(at.on_snapshot(id(1))), Some(("a".into(), id(1))));
        assert_eq!(at.on_session_exited(), Some("a".into()));
        assert_eq!(at.showing, None);
        assert!(!at.begin("b".into()));
        assert_eq!(tagged(at.on_snapshot(id(2))), Some(("b".into(), id(2))));
        assert_eq!(tagged(at.on_output()), Some(("b".into(), id(2))));
    }

    #[test]
    fn explicit_detach_drains_pending_snapshots() {
        let mut at = Attach::default();
        at.begin("a".into());
        assert_eq!(at.detach(), Some("a".into()));
        // Snapshot still in flight drains harmlessly — showing is None.
        assert_eq!(at.on_snapshot(id(1)), None);
    }

    #[test]
    fn revoking_the_shown_view_detaches_it() {
        let mut at = Attach::default();
        at.begin_view("a".into(), 7);
        at.on_snapshot(id(1));
        assert!(at.on_view_renamed(7, "a", "renamed"));

        assert_eq!(at.on_view_revoked(7), Some("renamed".into()));
        assert!(!at.is_attached());
    }

    #[test]
    fn stale_view_identity_cannot_rename_the_replacement_view() {
        let mut at = Attach::default();
        at.begin_view("replacement".into(), 8);
        at.on_snapshot(id(1));

        assert!(!at.on_view_renamed(7, "replacement", "wrong"));
        assert_eq!(tagged(at.on_output()), Some(("replacement".into(), id(1))));
    }

    #[test]
    fn duplicate_view_rename_is_not_forwarded_after_list_retag() {
        let mut at = Attach::default();
        at.begin_view("old".into(), 7);
        at.on_snapshot(id(1));
        at.on_rename("old", "new");

        assert!(!at.on_view_renamed(7, "old", "new"));
        assert_eq!(tagged(at.on_output()), Some(("new".into(), id(1))));
    }

    #[test]
    fn stale_revocation_does_not_cancel_a_pending_reattach() {
        let mut at = Attach::default();
        at.begin_view("a".into(), 7);
        at.on_snapshot(id(1));
        at.begin_view("a".into(), 8);

        assert_eq!(at.on_view_revoked(7), None);
        assert_eq!(tagged(at.on_snapshot(id(2))), Some(("a".into(), id(2))));
        assert!(at.is_attached());
    }
}
