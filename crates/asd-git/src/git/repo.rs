//! Opening a repository, and the handle everything else reads through.

use std::path::{Path, PathBuf};

use crate::git::commit::{CommitInfo, ReadError};

/// Why a path could not be shown as a graph. Each variant is a different thing
/// to tell the user, so the overlay never has to render a generic failure.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("{path} is not a git repository")]
    NotARepository { path: PathBuf },
    /// A repository was found but has no working tree (a bare repo, or one
    /// whose worktree is gone).
    #[error("{path} has no working tree")]
    NoWorkTree { path: PathBuf },
    /// A repository *was* found, but git's ownership check rejected it: the
    /// dubious-ownership case — a container, `sudo`, a mounted volume, a
    /// session running as another user. Calling that "not a git repository"
    /// would send the user hunting for a missing `.git` that is right there.
    ///
    /// Unreachable through `Repo::open` as it stands, and deliberately kept
    /// anyway. gix's default discovery policy is `Required(Trust::Reduced)`,
    /// and `Trust` has no level *below* `Reduced` — so an unowned repository
    /// clears the bar and simply opens with reduced trust. The variant is
    /// where the rejection must land the moment that policy is tightened, or
    /// gix changes its default; routing it into `NotARepository` in the
    /// meantime is how the wrong message gets written by accident later.
    #[error(
        "{path}: git does not trust this repository's ownership (see `safe.directory` in git-config)"
    )]
    Untrusted {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("opening {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Size of the in-memory object cache. `Sorting::ByCommitTime` looks each
/// commit up twice without one, and reading author/summary for a visible row is
/// a third lookup.
const OBJECT_CACHE_BYTES: usize = 4 * 1024 * 1024;

/// An open repository. Cheap to hold; the expensive state is gix's own caches.
pub struct Repo {
    inner: gix::Repository,
    workdir: PathBuf,
}

// `gix::Repository` does not implement `Debug`, so `#[derive(Debug)]` is not
// available here. A manual impl showing just the workdir is enough for the
// test assertions and any diagnostic printing to need.
impl std::fmt::Debug for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repo")
            .field("workdir", &self.workdir)
            .finish()
    }
}

impl Repo {
    /// Open the repository containing `path`, searching upwards for `.git`.
    pub fn open(path: &Path) -> Result<Self, OpenError> {
        let mut inner = gix::discover(path).map_err(|source| {
            use gix::discover::upwards::Error as Discover;
            let path = path.to_path_buf();
            match source {
                // Discovery failing is not one thing, and each kind sends the
                // user somewhere different. Only a genuine "searched upwards
                // and found nothing" is `NotARepository`.
                gix::discover::Error::Discover(e @ Discover::NoTrustedGitRepository { .. }) => {
                    OpenError::Untrusted {
                        path,
                        source: Box::new(e),
                    }
                }
                // Three filesystem failures, not three missing repositories.
                // `InaccessibleDirectory` is the one that actually bites: the
                // session's directory was deleted or is unreadable, and
                // "not a git repository" would have the user looking for a
                // `.git` when the problem is the path itself. `CheckTrust`
                // belongs here too despite its name — it is the `io::Error`
                // from stat-ing the candidate, not an ownership verdict.
                gix::discover::Error::Discover(
                    e @ (Discover::CurrentDir(_)
                    | Discover::InaccessibleDirectory { .. }
                    | Discover::CheckTrust { .. }),
                ) => OpenError::Io {
                    path,
                    source: Box::new(e),
                },
                gix::discover::Error::Discover(_) => OpenError::NotARepository { path },
                other => OpenError::Io {
                    path,
                    source: Box::new(other),
                },
            }
        })?;
        inner.object_cache_size_if_unset(OBJECT_CACHE_BYTES);
        let workdir = inner
            .workdir()
            .ok_or_else(|| OpenError::NoWorkTree {
                path: path.to_path_buf(),
            })?
            .to_path_buf();
        Ok(Self { inner, workdir })
    }

    /// The repository's working tree root. Used to tell whether a session
    /// switch landed in the same repository and the graph can be kept.
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub(crate) fn gix(&self) -> &gix::Repository {
        &self.inner
    }

    /// The object HEAD resolves to, or `None` in an unborn repository.
    pub fn head(&self) -> Option<gix::ObjectId> {
        self.inner.head_id().ok().map(|id| id.detach())
    }

    /// Every tip the graph should cover: HEAD plus all local and remote
    /// branches, so the walk shows the whole repository rather than one branch.
    fn tips(&self) -> Result<Vec<gix::ObjectId>, ReadError> {
        let mut tips: Vec<gix::ObjectId> = Vec::new();
        if let Some(head) = self.head() {
            tips.push(head);
        }
        for r in self.refs()? {
            if matches!(
                r.kind,
                crate::git::refs::RefKind::LocalBranch | crate::git::refs::RefKind::RemoteBranch
            ) {
                tips.push(r.target);
            }
        }
        tips.sort();
        tips.dedup();
        Ok(tips)
    }

    /// Walk the history newest-first.
    ///
    /// The iterator is lazy, but no caller exploits that: `GitGraph::reload`
    /// drains it in one `collect()` — see the comment there for why it has to
    /// — so in practice opening the overlay costs O(whole repository), one
    /// read of every commit reachable from every tip. That work happens on
    /// `asd ui`'s single render thread, the one that paints every session, and
    /// it was measured at 152 ms for 14 500 commits. Do not plan a feature
    /// around taking only the first *n* items being cheap: nothing takes only
    /// the first *n*.
    pub fn walk(
        &self,
    ) -> Result<impl Iterator<Item = Result<CommitInfo, ReadError>> + '_, ReadError> {
        use gix::revision::walk::Sorting;
        use gix::traverse::commit::simple::CommitTimeOrder;

        let tips = self.tips()?;
        let walk = self
            .inner
            .rev_walk(tips)
            .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
            .all()
            .map_err(|e| ReadError::from_err("walking history", e))?;

        Ok(walk.map(move |info| {
            let info = info.map_err(|e| ReadError::from_err("reading a commit", e))?;
            let commit = self
                .inner
                .find_commit(info.id)
                .map_err(|e| ReadError::from_err("finding a commit", e))?;
            let message = commit
                .message()
                .map_err(|e| ReadError::from_err("reading a commit message", e))?;
            let author = commit
                .author()
                .map_err(|e| ReadError::from_err("reading a commit author", e))?;
            let time = match info.commit_time {
                Some(t) => t,
                None => {
                    author
                        .time()
                        .map_err(|e| ReadError::from_err("reading a commit time", e))?
                        .seconds
                }
            };
            Ok(CommitInfo {
                id: info.id,
                parents: info.parent_ids.iter().copied().collect(),
                summary: message.summary().to_string(),
                author: author.name.to_string(),
                time,
            })
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::Fixture;

    #[test]
    fn opens_a_repository_and_reports_its_workdir() {
        let fx = Fixture::new("open");
        fx.commit("first");
        let repo = Repo::open(fx.path()).expect("fixture is a repository");
        // The fixture path and the reported workdir may differ by symlink
        // components (macOS /var -> /private/var), so compare canonically.
        assert_eq!(
            repo.workdir().canonicalize().unwrap(),
            fx.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn a_plain_directory_is_not_a_repository() {
        let dir = std::env::temp_dir().join(format!("asd-git-plain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = Repo::open(&dir).expect_err("a plain directory is not a repository");
        assert!(matches!(err, OpenError::NotARepository { .. }), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreadable_path_is_an_io_failure_not_a_missing_repository() {
        // `resolve_repo_path` reads a live session's cwd, which can be gone
        // by the time the overlay opens it. gix answers
        // `Discover::InaccessibleDirectory`, and reporting that as "not a git
        // repository" sends the user looking for a `.git` when the problem is
        // the path. A plain file stands in for the same class of failure.
        let dir = std::env::temp_dir().join(format!("asd-git-gone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("not-a-directory");
        std::fs::write(&file, b"").unwrap();

        let err = Repo::open(&file).expect_err("a file is not a directory to discover from");
        assert!(matches!(err, OpenError::Io { .. }), "{err:?}");

        let missing = dir.join("vanished");
        let err = Repo::open(&missing).expect_err("a path that does not exist cannot be opened");
        assert!(matches!(err, OpenError::Io { .. }), "{err:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opening_a_subdirectory_finds_the_repository_root() {
        let fx = Fixture::new("subdir");
        fx.commit("first");
        let nested = fx.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        let repo = Repo::open(&nested).expect("discovery walks upwards");
        assert_eq!(
            repo.workdir().canonicalize().unwrap(),
            fx.path().canonicalize().unwrap()
        );
    }
}
