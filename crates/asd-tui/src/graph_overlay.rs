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
        // A session whose pid is not known yet — created a moment ago, before
        // the list that carries the pid arrived — has nothing to follow to.
        // Leave the overlay on the repository it is already showing rather
        // than complaining about a directory nobody asked for.
        let Some(pid) = self.active_pid() else {
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
        if cwd.starts_with(current.workdir()) {
            return;
        }
        match open_at(&cwd) {
            Ok(graph) => self.git_graph = Some(graph),
            Err(_) => self.notice = Some("this session is not in a git repository".into()),
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
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_without_a_pid_cannot_be_resolved() {
        // pid 0 means "not known yet"; the overlay must say so rather than
        // opening the directory asd itself happens to be running in.
        assert!(matches!(
            resolve_repo_path(0),
            Err(OverlayError::UnknownDirectory)
        ));
    }

    // Windows' `session_cwd` is a documented `None`, so there is no pid this
    // can resolve there — including the test's own.
    #[cfg(unix)]
    #[test]
    fn this_process_resolves_to_its_own_directory() {
        let path = resolve_repo_path(std::process::id()).expect("own pid resolves");
        assert_eq!(
            path.canonicalize().unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
    }

    #[test]
    fn a_non_repository_reports_which_path_failed() {
        let dir = std::env::temp_dir().join(format!("asd-overlay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Matched rather than `expect_err`: that needs `T: Debug`, and
        // `GitGraph` does not implement it.
        let err = match open_at(&dir) {
            Ok(_) => panic!("a plain directory is not a repository: {}", dir.display()),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert!(
            msg.contains(&dir.display().to_string()),
            "the message names the path: {msg}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
