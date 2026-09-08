//! Single-writer session projection, bounded replay, and notification leases.
use asd_proto::{
    ClientKind, EventCursor, Frame, SessionEvent, SessionExit, SessionIdentity, SessionInfo,
    SessionUpdateCause, SessionUpdatePatch,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub enum CommittedSessionUpdate {
    Registered(SessionInfo),
    Updated {
        identity: SessionIdentity,
        patch: SessionUpdatePatch,
        cause: SessionUpdateCause,
    },
    Renamed {
        old_name: String,
        info: SessionInfo,
    },
    Exited {
        identity: SessionIdentity,
        last_name: String,
        exit: SessionExit,
    },
}

#[derive(Debug)]
pub struct HubClosed;

#[derive(Clone)]
pub struct EventPublisher {
    tx: mpsc::UnboundedSender<HubCommand>,
}

#[derive(Debug)]
pub struct EventSubscription {
    pub start: Frame,
    pub replay: Vec<Frame>,
    pub live: mpsc::Receiver<Frame>,
    _release: SubscriptionRelease,
}

#[derive(Debug)]
struct SubscriptionRelease {
    subscription_id: u64,
    tx: mpsc::UnboundedSender<HubCommand>,
}
impl Drop for SubscriptionRelease {
    fn drop(&mut self) {
        let _ = self.tx.send(HubCommand::Release(self.subscription_id));
    }
}

enum HubCommand {
    Publish(CommittedSessionUpdate),
    Track(SessionIdentity, Arc<crate::session::SessionMeta>),
    Subscribe {
        after: Option<EventCursor>,
        class: Option<ClientKind>,
        live: mpsc::Sender<Frame>,
        receiver: mpsc::Receiver<Frame>,
        release_tx: mpsc::UnboundedSender<HubCommand>,
        reply: oneshot::Sender<EventSubscription>,
    },
    Release(u64),
}

impl EventPublisher {
    pub fn new() -> Result<Self, getrandom::Error> {
        let mut epoch = [0; 16];
        getrandom::fill(&mut epoch)?;
        Ok(Self::with_epoch(epoch))
    }

    fn with_epoch(epoch: [u8; 16]) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let mut hub = SessionEventHub {
                cursor: EventCursor {
                    daemon_epoch: epoch,
                    sequence: 0,
                },
                projection: HashMap::new(),
                clocks: HashMap::new(),
                tombstones: HashSet::new(),
                replay: VecDeque::new(),
                subscribers: BTreeMap::new(),
                next_subscription: 0,
            };
            while let Some(command) = rx.blocking_recv() {
                hub.handle(command);
            }
        });
        Self { tx }
    }

    pub fn publish(&self, update: CommittedSessionUpdate) {
        if self.tx.send(HubCommand::Publish(update)).is_err() {
            tracing::warn!("event hub closed while publishing");
        }
    }

    pub(crate) fn track_activity(
        &self,
        identity: SessionIdentity,
        meta: Arc<crate::session::SessionMeta>,
    ) {
        let _ = self.tx.send(HubCommand::Track(identity, meta));
    }

    pub async fn subscribe(
        &self,
        after: Option<EventCursor>,
        kind: ClientKind,
        wants_notifications: bool,
    ) -> Result<EventSubscription, HubClosed> {
        let class = (wants_notifications && matches!(kind, ClientKind::Gui | ClientKind::Tui))
            .then_some(kind);
        let (live_tx, live) = mpsc::channel(64);
        let (reply, received) = oneshot::channel();
        self.tx
            .send(HubCommand::Subscribe {
                after,
                class,
                live: live_tx,
                receiver: live,
                release_tx: self.tx.clone(),
                reply,
            })
            .map_err(|_| HubClosed)?;
        received.await.map_err(|_| HubClosed)
    }
}

struct Subscriber {
    class: Option<ClientKind>,
    lease: bool,
    live: mpsc::Sender<Frame>,
}
struct SessionEventHub {
    cursor: EventCursor,
    projection: HashMap<SessionIdentity, SessionInfo>,
    clocks: HashMap<SessionIdentity, Arc<crate::session::SessionMeta>>,
    tombstones: HashSet<SessionIdentity>,
    replay: VecDeque<Frame>,
    subscribers: BTreeMap<u64, Subscriber>,
    next_subscription: u64,
}

impl SessionEventHub {
    fn handle(&mut self, command: HubCommand) {
        match command {
            HubCommand::Publish(update) => self.publish(update),
            HubCommand::Track(id, clock) => {
                if !self.tombstones.contains(&id) {
                    self.clocks.insert(id, clock);
                }
            }
            HubCommand::Release(id) => self.release(id),
            HubCommand::Subscribe {
                after,
                class,
                live,
                receiver,
                release_tx,
                reply,
            } => {
                // Prune canceled subscribe requests before deciding who holds a lease.
                let closed: Vec<_> = self
                    .subscribers
                    .iter()
                    .filter(|(_, sub)| sub.live.is_closed())
                    .map(|(id, _)| *id)
                    .collect();
                for id in closed {
                    self.release(id);
                }
                let valid = after.filter(|c| {
                    c.daemon_epoch == self.cursor.daemon_epoch
                        && c.sequence <= self.cursor.sequence
                        && self.cursor.sequence - c.sequence <= self.replay.len() as u64
                });
                let lease = class.is_some()
                    && !self
                        .subscribers
                        .values()
                        .any(|s| s.class == class && s.lease);
                let sessions = if valid.is_none() {
                    self.snapshot()
                } else {
                    Vec::new()
                };
                let start = Frame::EventStreamStarted {
                    cursor: valid.unwrap_or(self.cursor),
                    sessions,
                    reset: valid.is_none(),
                    notification_lease: lease,
                };
                let replay = valid.map(|c| self.replay.iter().filter(|f| matches!(f, Frame::SessionEvent { cursor, .. } if cursor.sequence > c.sequence)).cloned().collect()).unwrap_or_default();
                self.next_subscription += 1;
                let id = self.next_subscription;
                self.subscribers
                    .insert(id, Subscriber { class, lease, live });
                let subscription = EventSubscription {
                    start,
                    replay,
                    live: receiver,
                    _release: SubscriptionRelease {
                        subscription_id: id,
                        tx: release_tx,
                    },
                };
                if reply.send(subscription).is_err() {
                    self.release(id);
                }
            }
        }
    }

    fn snapshot(&self) -> Vec<SessionInfo> {
        let mut sessions: Vec<_> = self
            .projection
            .values()
            .cloned()
            .map(|mut info| {
                if let Some(meta) = self.clocks.get(&info.identity()) {
                    info.idle_ms = crate::session::now_ms().saturating_sub(
                        meta.last_output_ms
                            .load(std::sync::atomic::Ordering::Relaxed),
                    );
                    info.running = info.idle_ms < asd_proto::IDLE_SETTLE_MS;
                }
                info
            })
            .collect();
        sessions.sort_by(|a, b| a.name.cmp(&b.name).then(a.instance_id.cmp(&b.instance_id)));
        sessions
    }

    fn publish(&mut self, update: CommittedSessionUpdate) {
        let event = match update {
            CommittedSessionUpdate::Registered(info) => {
                let id = info.identity();
                if self.tombstones.contains(&id) || self.projection.contains_key(&id) {
                    return;
                }
                self.projection.insert(id, info.clone());
                SessionEvent::Registered { info }
            }
            CommittedSessionUpdate::Updated {
                identity,
                patch,
                cause,
            } => {
                let Some(info) = self.projection.get_mut(&identity) else {
                    return;
                };
                apply_patch(info, &patch);
                SessionEvent::Updated {
                    identity,
                    patch,
                    cause,
                }
            }
            CommittedSessionUpdate::Renamed { old_name, info } => {
                let Some(current) = self.projection.get_mut(&info.identity()) else {
                    return;
                };
                // Registry owns names; preserve all already-ordered session facts.
                current.name = info.name;
                SessionEvent::Renamed {
                    old_name,
                    info: current.clone(),
                }
            }
            CommittedSessionUpdate::Exited {
                identity,
                last_name,
                exit,
            } => {
                if self.projection.remove(&identity).is_none() {
                    return;
                }
                self.tombstones.insert(identity);
                self.clocks.remove(&identity);
                SessionEvent::Exited {
                    identity,
                    last_name,
                    exit,
                }
            }
        };
        self.cursor.sequence = self
            .cursor
            .sequence
            .checked_add(1)
            .expect("event sequence exhausted");
        let frame = Frame::SessionEvent {
            cursor: self.cursor,
            event,
        };
        self.replay.push_back(frame.clone());
        if self.replay.len() > 512 {
            self.replay.pop_front();
        }
        let failed: Vec<_> = self
            .subscribers
            .iter()
            .filter(|(_, s)| s.live.try_send(frame.clone()).is_err())
            .map(|(id, _)| *id)
            .collect();
        for id in failed {
            self.release(id);
        }
    }

    fn release(&mut self, id: u64) {
        let Some(old) = self.subscribers.remove(&id) else {
            return;
        };
        if !old.lease {
            return;
        }
        loop {
            let Some((&next, sub)) = self
                .subscribers
                .iter_mut()
                .find(|(_, s)| s.class == old.class)
            else {
                return;
            };
            if sub
                .live
                .try_send(Frame::NotificationLeaseChanged { granted: true })
                .is_ok()
            {
                sub.lease = true;
                return;
            }
            self.subscribers.remove(&next);
        }
    }
}

pub(crate) fn empty_patch() -> SessionUpdatePatch {
    SessionUpdatePatch {
        command: None,
        title: None,
        status_line: None,
        task: None,
        idle_ms: None,
        running: None,
        state: None,
        attached_clients: None,
        pid: None,
        cols: None,
        rows: None,
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
        AgentState, ClientKind, EventCursor, Frame, SessionExit, SessionInfo, SessionUpdateCause,
    };

    fn info(id: u128) -> SessionInfo {
        SessionInfo {
            name: format!("s{id}"),
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
    fn started(frame: &Frame, sequence: u64, reset: bool) {
        assert!(
            matches!(frame, Frame::EventStreamStarted { cursor: c, reset: r, .. } if *c == cursor(sequence) && *r == reset),
            "{frame:?}"
        );
    }

    #[tokio::test]
    async fn snapshot_refreshes_output_age_without_allocating_event_cursors() {
        check_sampled_activity(true, false).await;
    }

    #[tokio::test]
    async fn snapshot_refreshes_running_after_quiet_projection_receives_output() {
        check_sampled_activity(false, true).await;
    }

    async fn check_sampled_activity(projected_running: bool, sampled_running: bool) {
        use std::sync::{
            Mutex,
            atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicU64, Ordering},
        };
        let hub = EventPublisher::with_epoch([7; 16]);
        let meta = Arc::new(crate::session::SessionMeta {
            cols: AtomicU16::new(80),
            rows: AtomicU16::new(24),
            attached_clients: AtomicU32::new(0),
            child_pid: AtomicU32::new(1),
            alive: AtomicBool::new(true),
            pty_master_fd: AtomicI32::new(-1),
            title: Mutex::new(String::new()),
            status_line: Mutex::new(String::new()),
            task: Mutex::new(None),
            state: Mutex::new(AgentState::Unknown),
            last_output_ms: AtomicU64::new(crate::session::now_ms().saturating_sub(5000)),
            name: Mutex::new("s1".into()),
        });
        let mut registered = info(1);
        registered.state = AgentState::Working;
        registered.running = projected_running;
        registered.idle_ms = if projected_running { 0 } else { 5000 };
        hub.track_activity(registered.identity(), Arc::clone(&meta));
        hub.publish(CommittedSessionUpdate::Registered(registered));
        if sampled_running {
            meta.last_output_ms
                .store(crate::session::now_ms(), Ordering::Relaxed);
        }
        let snapshot = hub.subscribe(None, ClientKind::Cli, false).await.unwrap();
        started(&snapshot.start, 1, true);
        let Frame::EventStreamStarted { sessions, .. } = snapshot.start else {
            panic!("missing snapshot");
        };
        if sampled_running {
            assert!(sessions[0].idle_ms < asd_proto::IDLE_SETTLE_MS);
        } else {
            assert!(sessions[0].idle_ms >= 5000);
        }
        assert_eq!(sessions[0].running, sampled_running);
        assert_eq!(sessions[0].state, AgentState::Working);
    }

    #[tokio::test]
    async fn rename_keeps_ordered_owner_facts_and_late_patches_cannot_restore_name() {
        let hub = EventPublisher::with_epoch([7; 16]);
        let identity = info(1).identity();
        hub.publish(CommittedSessionUpdate::Registered(info(1)));
        let mut patch = empty_patch();
        patch.state = Some(AgentState::Working);
        patch.status_line = Some("ready".into());
        hub.publish(CommittedSessionUpdate::Updated {
            identity,
            patch,
            cause: SessionUpdateCause::ScreenDetection,
        });
        let mut renamed = info(1);
        renamed.name = "new".into();
        hub.publish(CommittedSessionUpdate::Renamed {
            old_name: "s1".into(),
            info: renamed,
        });
        let mut patch = empty_patch();
        patch.title = Some("late title".into());
        hub.publish(CommittedSessionUpdate::Updated {
            identity,
            patch,
            cause: SessionUpdateCause::ForegroundChanged,
        });
        let sub = hub.subscribe(None, ClientKind::Cli, false).await.unwrap();
        let Frame::EventStreamStarted { sessions, .. } = sub.start else {
            panic!("missing snapshot");
        };
        assert_eq!(sessions[0].name, "new");
        assert_eq!(sessions[0].state, AgentState::Working);
        assert_eq!(sessions[0].status_line, "ready");
        assert_eq!(sessions[0].title, "late title");
    }

    #[tokio::test]
    async fn replay_and_live_installation_share_one_atomic_boundary() {
        let hub = EventPublisher::with_epoch([7; 16]);
        hub.publish(CommittedSessionUpdate::Registered(info(1)));
        let publishing = hub.clone();
        let thread = std::thread::spawn(move || {
            for id in 2..=30 {
                publishing.publish(CommittedSessionUpdate::Registered(info(id)));
            }
        });
        let mut sub = hub
            .subscribe(Some(cursor(1)), ClientKind::Cli, false)
            .await
            .unwrap();
        thread.join().unwrap();
        started(&sub.start, 1, false);
        let mut frames = std::mem::take(&mut sub.replay);
        while frames.len() < 29 {
            frames.push(sub.live.recv().await.unwrap());
        }
        for (frame, sequence) in frames.iter().zip(2..=30) {
            assert!(
                matches!(frame, Frame::SessionEvent { cursor: c, .. } if *c == cursor(sequence))
            );
        }
    }
    #[tokio::test]
    async fn snapshot_live_boundary_and_replay_are_contiguous() {
        let hub = EventPublisher::with_epoch([7; 16]);
        let mut sub = hub.subscribe(None, ClientKind::Cli, true).await.unwrap();
        started(&sub.start, 0, true);
        for id in 1..=3 {
            hub.publish(CommittedSessionUpdate::Registered(info(id)));
        }
        for sequence in 1..=3 {
            assert!(
                matches!(sub.live.recv().await, Some(Frame::SessionEvent { cursor: c, .. }) if c == cursor(sequence))
            );
        }
        let replay = hub
            .subscribe(Some(cursor(1)), ClientKind::Cli, false)
            .await
            .unwrap();
        started(&replay.start, 1, false);
        assert_eq!(replay.replay.len(), 2);
        for (frame, sequence) in replay.replay.iter().zip([2, 3]) {
            assert!(
                matches!(frame, Frame::SessionEvent { cursor: c, .. } if *c == cursor(sequence))
            );
        }
    }
    #[tokio::test]
    async fn expired_foreign_and_future_cursors_reset() {
        let hub = EventPublisher::with_epoch([7; 16]);
        for id in 1..=514 {
            hub.publish(CommittedSessionUpdate::Registered(info(id)));
        }
        for after in [
            cursor(1),
            cursor(515),
            EventCursor {
                daemon_epoch: [8; 16],
                sequence: 514,
            },
        ] {
            let sub = hub
                .subscribe(Some(after), ClientKind::Cli, false)
                .await
                .unwrap();
            started(&sub.start, 514, true);
            assert!(sub.replay.is_empty());
        }
    }
    #[tokio::test]
    async fn tombstone_rejects_late_update_and_registration() {
        let hub = EventPublisher::with_epoch([7; 16]);
        let identity = info(1).identity();
        hub.publish(CommittedSessionUpdate::Registered(info(1)));
        hub.publish(CommittedSessionUpdate::Exited {
            identity,
            last_name: "s1".into(),
            exit: SessionExit {
                code: 0,
                signal: None,
            },
        });
        hub.publish(CommittedSessionUpdate::Updated {
            identity,
            patch: empty_patch(),
            cause: SessionUpdateCause::ScreenDetection,
        });
        hub.publish(CommittedSessionUpdate::Registered(info(1)));
        let sub = hub.subscribe(None, ClientKind::Cli, false).await.unwrap();
        started(&sub.start, 2, true);
        assert!(
            matches!(sub.start, Frame::EventStreamStarted { sessions, .. } if sessions.is_empty())
        );
    }
    #[tokio::test]
    async fn overflow_closes_after_contiguous_64_frames() {
        let hub = EventPublisher::with_epoch([7; 16]);
        let mut sub = hub.subscribe(None, ClientKind::Cli, false).await.unwrap();
        for id in 1..=65 {
            hub.publish(CommittedSessionUpdate::Registered(info(id)));
        }
        let barrier = hub.subscribe(None, ClientKind::Cli, false).await.unwrap();
        started(&barrier.start, 65, true);
        for sequence in 1..=64 {
            assert!(
                matches!(sub.live.recv().await, Some(Frame::SessionEvent { cursor: c, .. }) if c == cursor(sequence))
            );
        }
        assert!(sub.live.recv().await.is_none());
    }
    #[tokio::test]
    async fn leases_are_class_local_and_transfer_to_oldest_waiter() {
        let hub = EventPublisher::with_epoch([7; 16]);
        let gui = hub.subscribe(None, ClientKind::Gui, true).await.unwrap();
        let mut second = hub.subscribe(None, ClientKind::Gui, true).await.unwrap();
        let third = hub.subscribe(None, ClientKind::Gui, true).await.unwrap();
        let tui = hub.subscribe(None, ClientKind::Tui, true).await.unwrap();
        let cli = hub.subscribe(None, ClientKind::Cli, true).await.unwrap();
        for sub in [&gui, &tui] {
            assert!(matches!(
                sub.start,
                Frame::EventStreamStarted {
                    notification_lease: true,
                    ..
                }
            ));
        }
        for sub in [&second, &third, &cli] {
            assert!(matches!(
                sub.start,
                Frame::EventStreamStarted {
                    notification_lease: false,
                    ..
                }
            ));
        }
        drop(gui);
        assert!(matches!(
            second.live.recv().await,
            Some(Frame::NotificationLeaseChanged { granted: true })
        ));
    }
}
