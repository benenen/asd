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

use ratatui::style::Style;

use crate::git::diff::{CommitDiff, DiffLine, FileDiff};
use crate::git::repo::{OpenError, Repo};
use crate::ui::highlight::Highlighter;

/// How many unchanged lines a file diff keeps around each change.
pub(crate) const DIFF_CONTEXT: u32 = 3;

/// How many of a file diff's lines are syntax-highlighted.
///
/// Highlighting is ~141 us per line in a release build, so the
/// [`crate::git::diff::MAX_DIFF_LINES`] ceiling of 20 000 would be nearly
/// three seconds of worker time before the view could show anything. Past this
/// many lines the rest of the diff is carried unstyled — still numbered, still
/// readable, just not coloured.
pub(crate) const MAX_HIGHLIGHT_LINES: usize = 5_000;

/// Work for the diff thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    /// The changed-file list and totals for one commit.
    Commit(gix::ObjectId),
    /// One file's diff within one commit.
    File { commit: gix::ObjectId, path: String },
}

/// A finished computation. Errors are carried as text because they cross a
/// thread boundary and are only ever shown to the user.
#[derive(Debug)]
pub(crate) enum Reply {
    Commit {
        id: gix::ObjectId,
        result: Result<CommitDiff, String>,
    },
    File {
        commit: gix::ObjectId,
        path: String,
        result: Result<HighlightedDiff, String>,
    },
}

/// One file's diff with its lines already syntax-highlighted.
///
/// The styles are computed here, on the worker thread, and never in the view:
/// highlighting is ~141 us per line, so a 60-line screenful is 23 ms, and
/// `asd ui` paints every open session from the thread that draws this. Paying
/// it per frame would freeze all of them for as long as the viewer is open.
///
/// This is why the type lives at the crate root rather than in `git/`: it
/// carries `ratatui::style::Style`, and `git/` stays ratatui-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightedDiff {
    pub diff: FileDiff,
    /// Spans for `diff.lines`, in the same order and by the same index. Each
    /// line's spans concatenate back to that line's text.
    ///
    /// Shorter than `diff.lines` when the diff ran past
    /// `MAX_HIGHLIGHT_LINES`, and empty for a binary diff. A line with no
    /// entry is painted unstyled, so the view must index this with `get`.
    pub spans: Vec<Vec<(Style, String)>>,
    /// Display width of the widest line number in `diff`, which is how wide
    /// the viewer's gutter has to be.
    ///
    /// Sized against the whole diff, not the visible window, so the gutter
    /// does not jump about as the reader scrolls — and measured here, once,
    /// for the same reason `spans` is. It is a pure function of the diff, and
    /// deriving it in the view walked every line of a diff up to
    /// [`crate::git::diff::MAX_DIFF_LINES`] long, per frame, on the thread
    /// that paints every open session.
    pub num_w: usize,
}

/// Decimal digits in `n`, without allocating.
fn digits(n: u32) -> usize {
    n.checked_ilog10().unwrap_or(0) as usize + 1
}

impl HighlightedDiff {
    /// Pair a diff with the spans computed for it, measuring the gutter once.
    pub fn new(diff: FileDiff, spans: Vec<Vec<(Style, String)>>) -> Self {
        let num_w = diff
            .lines
            .iter()
            .map(|l| match l {
                DiffLine::Context { old, new, .. } => digits(*old).max(digits(*new)),
                DiffLine::Added { new, .. } => digits(*new),
                DiffLine::Removed { old, .. } => digits(*old),
            })
            .max()
            // An empty diff still needs a one-column gutter to line up with.
            .unwrap_or(1);
        Self { diff, spans, num_w }
    }
}

/// Owns the thread. Dropping it closes the request channel, which is how the
/// thread learns to exit; a resident UI must not leave threads behind.
pub(crate) struct DiffWorker {
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
    pub(crate) fn new(path: &Path) -> Result<Self, OpenError> {
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
    pub(crate) fn request(&mut self, req: Request) {
        if self.tx.send(req).is_err() {
            self.alive = false;
        }
    }

    /// Take every finished reply. Never blocks.
    pub(crate) fn drain(&mut self) -> Vec<Reply> {
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
    pub(crate) fn is_alive(&self) -> bool {
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
    // One highlighter per side of the diff. Added lines are the new file's and
    // removed lines are the old file's, so a single parse state would feed the
    // two files' tokens to each other — a removed `/*` would colour the added
    // lines after it as a comment. Both are built here rather than per request
    // so the syntax and theme dumps are deserialised once, off the render
    // thread, before the first file is ever asked for.
    let mut new_side = Highlighter::new();
    let mut old_side = Highlighter::new();
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
                    .map_err(|e| e.to_string())
                    .map(|diff| highlight(diff, &mut new_side, &mut old_side)),
                path,
            },
        };
        if replies.send(reply).is_err() {
            return; // The owner is gone.
        }
    }
}

/// Highlight a whole file diff once, here, so the view never has to.
///
/// A context line belongs to both files, so it is fed to both highlighters —
/// the new side's colours are the ones shown, and the old side's call is what
/// keeps its parse state in step for the removed lines that follow.
fn highlight(
    diff: FileDiff,
    new_side: &mut Highlighter,
    old_side: &mut Highlighter,
) -> HighlightedDiff {
    // A new file starts a new parse: syntect carries state across lines, and
    // the previous file's state would otherwise colour this one's first lines.
    new_side.reset();
    old_side.reset();
    let mut spans = Vec::with_capacity(diff.lines.len().min(MAX_HIGHLIGHT_LINES));
    for line in diff.lines.iter().take(MAX_HIGHLIGHT_LINES) {
        spans.push(match line {
            DiffLine::Context { text, .. } => {
                let shown = new_side.line(&diff.path, text);
                let _ = old_side.line(&diff.path, text);
                shown
            }
            DiffLine::Added { text, .. } => new_side.line(&diff.path, text),
            DiffLine::Removed { text, .. } => old_side.line(&diff.path, text),
        });
    }
    HighlightedDiff::new(diff, spans)
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
                let h = result.expect("diff computed");
                assert!(!h.diff.lines.is_empty());
                assert_eq!(
                    h.spans.len(),
                    h.diff.lines.len(),
                    "every line comes back with its spans"
                );
                for (line, spans) in h.diff.lines.iter().zip(&h.spans) {
                    let text = match line {
                        DiffLine::Context { text, .. }
                        | DiffLine::Added { text, .. }
                        | DiffLine::Removed { text, .. } => text,
                    };
                    let joined: String = spans.iter().map(|(_, t)| t.as_str()).collect();
                    assert_eq!(&joined, text, "spans must reproduce their line");
                }
            }
            other => panic!("expected a file reply, got {other:?}"),
        }
    }

    #[test]
    fn a_source_file_comes_back_already_highlighted() {
        // The render thread must never call the highlighter: ~141 us per line
        // is 23 ms for one screenful, on the thread that paints every open
        // session. The worker is where that cost is paid, so the reply has to
        // carry real colours rather than a promise of them.
        let fx = Fixture::new("worker-highlight");
        write_commit(&fx, "a.rs", "fn main() {}\n", "first");
        let id: gix::ObjectId = write_commit(
            &fx,
            "a.rs",
            "fn main() { let x = 1; }\nstruct S;\n",
            "second",
        )
        .parse()
        .unwrap();

        let mut w = DiffWorker::new(fx.path()).unwrap();
        w.request(Request::File {
            commit: id,
            path: "a.rs".into(),
        });
        let reply = wait_for(|| w.drain().into_iter().next());
        let Reply::File { result, .. } = reply else {
            panic!("expected a file reply");
        };
        let h = result.expect("diff computed");
        let added = h
            .diff
            .lines
            .iter()
            .position(|l| matches!(l, DiffLine::Added { .. }))
            .expect("the second commit added a line");
        let spans = &h.spans[added];
        assert!(
            spans.len() > 1,
            "a Rust line must be split into several spans, got {spans:?}"
        );
        assert!(
            spans.iter().any(|(s, _)| s.fg.is_some()),
            "and must carry colours: {spans:?}"
        );
    }

    #[test]
    fn the_two_sides_of_a_diff_are_highlighted_independently() {
        // A removed line that opens a block comment belongs to the OLD file.
        // With one shared parse state the added lines after it are painted as
        // if they were inside that comment.
        let fx = Fixture::new("worker-two-sides");
        write_commit(&fx, "a.rs", "/* still open\nlet a = 1;\n", "first");
        let id: gix::ObjectId = write_commit(&fx, "a.rs", "let b = 2;\nlet a = 1;\n", "second")
            .parse()
            .unwrap();

        let mut w = DiffWorker::new(fx.path()).unwrap();
        w.request(Request::File {
            commit: id,
            path: "a.rs".into(),
        });
        let reply = wait_for(|| w.drain().into_iter().next());
        let Reply::File { result, .. } = reply else {
            panic!("expected a file reply");
        };
        let h = result.expect("diff computed");

        let mut fresh = Highlighter::new();
        let added = h
            .diff
            .lines
            .iter()
            .zip(&h.spans)
            .find_map(|(l, s)| match l {
                DiffLine::Added { text, .. } => Some((text, s)),
                _ => None,
            })
            .expect("the second commit added a line");
        let (text, spans) = added;
        assert_eq!(
            spans,
            &fresh.line("a.rs", text),
            "an added line must be coloured as the new file's, not as the \
             continuation of a comment only the old file opened"
        );
    }

    #[test]
    fn a_diff_longer_than_the_highlight_limit_keeps_its_lines() {
        // The cap bounds the worker's own latency, not the diff: the lines
        // past it are still delivered, just without spans, and the view paints
        // them from their own text.
        let lines: Vec<DiffLine> = (0..MAX_HIGHLIGHT_LINES + 10)
            .map(|i| DiffLine::Added {
                new: i as u32 + 1,
                text: format!("line {i}"),
            })
            .collect();
        let want = lines.len();
        // An extension with no syntax keeps this cheap: the highlighter
        // short-circuits to one unstyled span per line.
        let diff = FileDiff {
            path: "a.zzzz".to_string(),
            lines,
            binary: false,
            truncated: false,
        };
        let h = highlight(diff, &mut Highlighter::new(), &mut Highlighter::new());
        assert_eq!(h.diff.lines.len(), want);
        assert_eq!(h.spans.len(), MAX_HIGHLIGHT_LINES);
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
