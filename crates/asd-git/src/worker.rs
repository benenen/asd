//! A plain background thread that computes diffs.
//!
//! `asd ui` paints every terminal session from one thread. A diff computed on
//! that thread freezes all of them, so every diff goes through here instead.
//! Deliberately `std::thread` and channels rather than an async runtime: this
//! crate knows nothing about the host's runtime and must not pull one in.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use crate::git::diff::{CommitDiff, FileDiff};
use crate::git::repo::{OpenError, Repo};

/// How many unchanged lines a file diff keeps around each change.
pub const DIFF_CONTEXT: u32 = 3;

/// Work for the diff thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// The changed-file list and totals for one commit.
    Commit(gix::ObjectId),
    /// One file's diff within one commit.
    File { commit: gix::ObjectId, path: String },
}

/// A finished computation. Errors are carried as text because they cross a
/// thread boundary and are only ever shown to the user.
#[derive(Debug)]
pub enum Reply {
    Commit {
        id: gix::ObjectId,
        result: Result<CommitDiff, String>,
    },
    File {
        commit: gix::ObjectId,
        path: String,
        result: Result<FileDiff, String>,
    },
}

/// Owns the thread. Dropping it closes the request channel, which is how the
/// thread learns to exit; a resident UI must not leave threads behind.
pub struct DiffWorker {
    tx: Sender<Request>,
    rx: Receiver<Reply>,
    // Only read by the `#[cfg(test)]` accessor below, which is how
    // `dropping_the_worker_stops_its_thread` observes the thread exiting.
    // Outside tests nothing reads it, so it is dead code in that build.
    #[cfg_attr(not(test), allow(dead_code))]
    finished: Arc<AtomicBool>,
    alive: bool,
}

impl std::fmt::Debug for DiffWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffWorker")
            .field("alive", &self.alive)
            .finish()
    }
}

impl DiffWorker {
    /// Open `path` on a new thread and start serving requests.
    ///
    /// The repository is opened on the worker thread's own handle: `gix`'s
    /// `Repository` is not shared across threads here, so each side owns one.
    pub fn new(path: &Path) -> Result<Self, OpenError> {
        // Fail fast on the caller's thread if the path is not a repository, so
        // the error reaches the user as an open failure rather than as a dead
        // worker.
        let repo = Repo::open(path)?;
        let (tx, work_rx) = channel::<Request>();
        let (reply_tx, rx) = channel::<Reply>();
        let finished = Arc::new(AtomicBool::new(false));
        let finished_thread = Arc::clone(&finished);

        std::thread::Builder::new()
            .name("asd-git-diff".into())
            .spawn(move || {
                serve(repo, &work_rx, &reply_tx);
                finished_thread.store(true, Ordering::SeqCst);
            })
            .map_err(|e| OpenError::Io {
                path: path.to_path_buf(),
                source: Box::new(e),
            })?;

        Ok(Self {
            tx,
            rx,
            finished,
            alive: true,
        })
    }

    /// Post work. A closed channel means the thread died; the worker records
    /// that and later requests are dropped rather than panicking.
    pub fn request(&mut self, req: Request) {
        if self.tx.send(req).is_err() {
            self.alive = false;
        }
    }

    /// Take every finished reply. Never blocks.
    pub fn drain(&mut self) -> Vec<Reply> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(reply) => out.push(reply),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.alive = false;
                    break;
                }
            }
        }
        out
    }

    /// False once the thread has gone. The caller shows "diffs unavailable"
    /// and keeps the rest of the overlay working.
    pub fn is_alive(&self) -> bool {
        self.alive
    }

    #[cfg(test)]
    pub(crate) fn thread_finished_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.finished)
    }

    /// Close the request side of the channel without touching the reply
    /// side, so a reply already sent by the background thread stays sitting
    /// in `rx`, retrievable by `drain`, after the thread notices the closed
    /// request channel and exits.
    ///
    /// This lets a test reproduce, deterministically rather than by racing
    /// real timing, the exact sequence `poll` must handle correctly: the
    /// worker thread finishing a request, sending its reply, and then dying,
    /// all before the reply is drained. Assigning over `self.tx` drops the
    /// original sender, which is what makes the background thread's next
    /// `recv` on the paired receiver fail; any request already queued ahead
    /// of that point is still delivered first, so this does not lose work
    /// that was already accepted.
    #[cfg(test)]
    pub(crate) fn close_requests_for_test(&mut self) {
        let (tx, rx) = channel::<Request>();
        drop(rx);
        self.tx = tx;
    }
}

/// The thread body. Returns when the request channel closes.
fn serve(repo: Repo, work: &Receiver<Request>, replies: &Sender<Reply>) {
    while let Ok(req) = work.recv() {
        let reply = match req {
            Request::Commit(id) => Reply::Commit {
                id,
                result: repo.commit_diff(id).map_err(|e| e.to_string()),
            },
            Request::File { commit, path } => Reply::File {
                commit,
                result: repo
                    .file_diff(commit, &path, DIFF_CONTEXT)
                    .map_err(|e| e.to_string()),
                path,
            },
        };
        if replies.send(reply).is_err() {
            return; // The owner is gone.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::Fixture;

    /// Block until `f` returns something or the deadline passes. The worker is
    /// a real thread, so tests wait rather than sleeping a fixed amount.
    fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker never answered"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn write_commit(fx: &Fixture, name: &str, body: &str, summary: &str) -> String {
        std::fs::write(fx.path().join(name), body).unwrap();
        fx.git(&["add", name]);
        fx.commit(summary)
    }

    #[test]
    fn a_commit_request_comes_back_as_a_commit_reply() {
        let fx = Fixture::new("worker-commit");
        let id: gix::ObjectId = write_commit(&fx, "a.txt", "one\ntwo\n", "first")
            .parse()
            .unwrap();

        let mut w = DiffWorker::new(fx.path()).unwrap();
        w.request(Request::Commit(id));

        let reply = wait_for(|| w.drain().into_iter().next());
        match reply {
            Reply::Commit { id: got, result } => {
                assert_eq!(got, id);
                let d = result.expect("diff computed");
                assert_eq!(d.files.len(), 1);
                assert_eq!(d.files[0].path, "a.txt");
            }
            other => panic!("expected a commit reply, got {other:?}"),
        }
    }

    #[test]
    fn a_file_request_comes_back_with_that_files_lines() {
        let fx = Fixture::new("worker-file");
        write_commit(&fx, "a.txt", "1\n2\n3\n", "first");
        let id: gix::ObjectId = write_commit(&fx, "a.txt", "1\nTWO\n3\n", "second")
            .parse()
            .unwrap();

        let mut w = DiffWorker::new(fx.path()).unwrap();
        w.request(Request::File {
            commit: id,
            path: "a.txt".into(),
        });

        let reply = wait_for(|| w.drain().into_iter().next());
        match reply {
            Reply::File { path, result, .. } => {
                assert_eq!(path, "a.txt");
                let d = result.expect("diff computed");
                assert!(!d.lines.is_empty());
            }
            other => panic!("expected a file reply, got {other:?}"),
        }
    }

    #[test]
    fn an_error_arrives_as_a_message_rather_than_killing_the_worker() {
        let fx = Fixture::new("worker-error");
        let id: gix::ObjectId = write_commit(&fx, "a.txt", "one\n", "first")
            .parse()
            .unwrap();

        let mut w = DiffWorker::new(fx.path()).unwrap();
        w.request(Request::File {
            commit: id,
            path: "nope.txt".into(),
        });
        let reply = wait_for(|| w.drain().into_iter().next());
        match reply {
            Reply::File { result, .. } => {
                assert!(result.is_err(), "a missing path must be an error");
            }
            other => panic!("expected a file reply, got {other:?}"),
        }

        // The worker survives and still answers.
        assert!(w.is_alive());
        w.request(Request::Commit(id));
        let reply = wait_for(|| w.drain().into_iter().next());
        assert!(matches!(reply, Reply::Commit { .. }));
    }

    #[test]
    fn drain_is_empty_when_nothing_was_requested() {
        let fx = Fixture::new("worker-idle");
        write_commit(&fx, "a.txt", "one\n", "first");
        let mut w = DiffWorker::new(fx.path()).unwrap();
        assert!(w.drain().is_empty());
    }

    #[test]
    fn dropping_the_worker_stops_its_thread() {
        let fx = Fixture::new("worker-drop");
        write_commit(&fx, "a.txt", "one\n", "first");
        let w = DiffWorker::new(fx.path()).unwrap();
        let handle = w.thread_finished_flag();
        drop(w);
        // The thread notices the closed channel and exits.
        wait_for(|| {
            handle
                .load(std::sync::atomic::Ordering::SeqCst)
                .then_some(())
        });
    }
}
