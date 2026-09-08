//! Manifest preparation and session acknowledgement never hold the Registry lock.

use std::sync::{Arc, Mutex, atomic::Ordering};
use std::time::Duration;

use asd_proto::{Frame, SessionIdentity};
use tokio::sync::oneshot;

use super::Registry;
use crate::detect::PreparedDetectorReload;
use crate::session::{SessionHandle, SessionMsg};

impl Registry {
    pub async fn reload_detectors(registry: Arc<Mutex<Self>>) -> anyhow::Result<Frame> {
        let gate = registry.lock().unwrap().reload_gate.clone();
        let guard = gate.lock_owned().await;
        // The gate spans preparation and installation, including cancellation
        // of the requesting connection: spawn_blocking owns the guard.
        let (generation, diagnostics, acknowledgements) = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let store = registry.lock().unwrap().detectors.clone();
            let prepared: PreparedDetectorReload = store.reload_candidate();
            let diagnostics = prepared.diagnostics.clone();
            let mut reg = registry.lock().unwrap();
            let snapshot = reg.detectors.install(prepared);
            reg.context.detector = snapshot.clone();
            let handles: Vec<_> = reg.sessions.values().cloned().collect();
            drop(reg);
            // Queue the committed generation even if the requesting task was
            // cancelled while preparation was running on this blocking worker.
            let mut acknowledgements = Vec::new();
            for handle in handles {
                let (ack, receipt) = oneshot::channel();
                let _ = handle.tx.send(SessionMsg::DetectorReloaded {
                    generation: snapshot.generation,
                    detector: snapshot.detector.clone(),
                    ack,
                });
                acknowledgements.push((handle, receipt));
            }
            (snapshot.generation, diagnostics, acknowledgements)
        })
        .await?;

        let pending_identities =
            pending_reloads(generation, acknowledgements, Duration::from_secs(5)).await;
        Ok(Frame::AgentManifestsReloaded {
            generation,
            diagnostics,
            pending_identities,
        })
    }
}

async fn pending_reloads(
    generation: u64,
    acknowledgements: Vec<(SessionHandle, oneshot::Receiver<u64>)>,
    timeout: Duration,
) -> Vec<SessionIdentity> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut pending = Vec::new();
    for (handle, receipt) in acknowledgements {
        if !handle.meta.alive.load(Ordering::Acquire) {
            continue;
        }
        let acknowledged = matches!(tokio::time::timeout_at(deadline, receipt).await, Ok(Ok(applied)) if applied >= generation);
        if !acknowledged {
            pending.push(handle);
        }
    }
    let mut pending: Vec<_> = pending
        .into_iter()
        .filter(|handle| handle.meta.alive.load(Ordering::Acquire))
        .map(|handle| handle.identity)
        .collect();
    pending.sort_by_key(|identity| identity.instance_id);
    pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::DetectorStore;
    use crate::session::SessionMeta;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicU64};

    fn session(id: u128) -> (SessionHandle, std::sync::mpsc::Receiver<SessionMsg>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            SessionHandle {
                name: format!("test{id}"),
                identity: SessionIdentity { instance_id: id },
                command: "sh".into(),
                spawn_command: None,
                created_ms: 0,
                tx,
                meta: Arc::new(SessionMeta {
                    cols: AtomicU16::new(80),
                    rows: AtomicU16::new(24),
                    attached_clients: AtomicU32::new(0),
                    child_pid: AtomicU32::new(0),
                    alive: AtomicBool::new(true),
                    pty_master_fd: AtomicI32::new(-1),
                    title: Mutex::new(String::new()),
                    status_line: Mutex::new(String::new()),
                    state: Mutex::new(asd_proto::AgentState::Unknown),
                    last_output_ms: AtomicU64::new(0),
                    name: Mutex::new(format!("test{id}")),
                }),
            },
            rx,
        )
    }

    fn registry(tag: &str) -> (Arc<Mutex<Registry>>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("asd-reload-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store =
            crate::store::SessionStore::open(dir.join("sessions.json"), dir.join("sessions.tsv"))
                .unwrap()
                .store;
        let mut reg = Registry::new(0, store, Vec::new(), dir.join("asd.sock"));
        reg.detectors = DetectorStore::load(dir.join("agents"));
        reg.context.detector = reg.detectors.snapshot();
        (Arc::new(Mutex::new(reg)), dir)
    }

    #[tokio::test]
    async fn reload_deadline_reports_laggards_without_blocking_registry_or_rolling_back() {
        let (registry, dir) = registry("deadline");
        let (handle, rx) = session(42);
        registry
            .lock()
            .unwrap()
            .sessions
            .insert(handle.name.clone(), handle);
        let started = tokio::time::Instant::now();
        let reload = tokio::spawn(Registry::reload_detectors(registry.clone()));
        let message =
            tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(3)).unwrap())
                .await
                .unwrap();
        let SessionMsg::DetectorReloaded {
            generation,
            detector: _,
            ack,
        } = message
        else {
            panic!("wrong session message");
        };
        let generation_before_reply = registry
            .try_lock()
            .expect("Registry lock held while awaiting session")
            .context
            .detector
            .generation;
        assert_eq!(generation_before_reply, generation);
        let report = tokio::time::timeout(Duration::from_secs(8), reload)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(started.elapsed() >= Duration::from_secs(5));
        assert_eq!(
            report,
            Frame::AgentManifestsReloaded {
                generation,
                diagnostics: Vec::new(),
                pending_identities: vec![SessionIdentity { instance_id: 42 }],
            }
        );
        assert_eq!(
            registry.lock().unwrap().context.detector.generation,
            generation
        );
        assert!(
            ack.send(generation).is_err(),
            "timed-out reply receiver should be dropped"
        );
        drop(registry);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn reload_barrier_accepts_newer_ack_and_exits_but_not_older_ack() {
        let mut receipts = Vec::new();
        for (id, generation) in [(1, 4), (2, 5), (3, 6)] {
            let (handle, _rx) = session(id);
            let (ack, receipt) = oneshot::channel();
            ack.send(generation).unwrap();
            receipts.push((handle, receipt));
        }
        let (exiting, _rx) = session(4);
        let meta = exiting.meta.clone();
        let (ack, receipt) = oneshot::channel();
        receipts.push((exiting, receipt));
        tokio::spawn(async move {
            meta.alive.store(false, Ordering::Release);
            drop(ack);
        });
        assert_eq!(
            pending_reloads(5, receipts, Duration::from_secs(1)).await,
            vec![SessionIdentity { instance_id: 1 }]
        );
    }

    #[tokio::test]
    async fn concurrent_reloads_install_distinct_monotonic_generations() {
        let (registry, dir) = registry("concurrent");
        let initial = registry.lock().unwrap().context.detector.generation;
        let (first, second) = tokio::join!(
            Registry::reload_detectors(registry.clone()),
            Registry::reload_detectors(registry.clone())
        );
        let mut generations = Vec::new();
        for result in [first, second] {
            let Frame::AgentManifestsReloaded {
                generation,
                pending_identities,
                ..
            } = result.unwrap()
            else {
                panic!("wrong reload reply");
            };
            assert!(pending_identities.is_empty());
            generations.push(generation);
        }
        generations.sort_unstable();
        assert_eq!(generations, [initial + 1, initial + 2]);
        assert_eq!(
            registry.lock().unwrap().context.detector.generation,
            initial + 2
        );
        drop(registry);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
