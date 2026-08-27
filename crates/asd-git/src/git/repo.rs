//! Opening a repository, and the handle everything else reads through.

use std::path::{Path, PathBuf};

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
    // Read only through `gix()`, which later tasks (commit reading, ref
    // reading) call; unused for now since phase 1 stops at discovery.
    #[allow(dead_code)]
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
        let mut inner = gix::discover(path).map_err(|source| match source {
            gix::discover::Error::Discover(_) => OpenError::NotARepository {
                path: path.to_path_buf(),
            },
            other => OpenError::Io {
                path: path.to_path_buf(),
                source: Box::new(other),
            },
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

    // Unused until later tasks add commit/ref reading on top of this handle.
    #[allow(dead_code)]
    pub(crate) fn gix(&self) -> &gix::Repository {
        &self.inner
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
