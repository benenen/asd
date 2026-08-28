//! How much uncommitted work the tree is carrying.
//!
//! Only a count: the overlay shows one synthetic row above the newest commit,
//! and phase 1's contract is read-only, so nothing here stages or writes.

use crate::git::commit::ReadError;
use crate::git::repo::Repo;

impl Repo {
    /// Modified and untracked entries in the working tree.
    ///
    /// Costs a working-tree walk (12 ms on this repository), so it belongs off
    /// the render thread with everything else that touches the filesystem.
    ///
    /// An unborn repository (no commits yet, so no HEAD to diff the index
    /// against) is not special-cased: `gix`'s status iterator handles a
    /// missing HEAD on its own and simply walks the index and the worktree,
    /// so an unborn repository with staged or untracked files is counted like
    /// any other, and one with neither reports `Ok(0)`. Verified directly
    /// against fixtures in both states below, not just the empty case.
    pub fn working_changes(&self) -> Result<usize, ReadError> {
        let status = self
            .gix()
            .status(gix::progress::Discard)
            .map_err(|e| ReadError::from_err("opening status", e))?
            .index_worktree_submodules(gix::status::Submodule::AsConfigured { check_dirty: false })
            .into_iter(None)
            .map_err(|e| ReadError::from_err("starting status", e))?;

        let mut n = 0usize;
        for item in status {
            // A single unreadable entry should not blank the count.
            if item.is_ok() {
                n += 1;
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use crate::git::fixture::Fixture;
    use crate::git::repo::Repo;

    #[test]
    fn a_clean_tree_reports_no_changes() {
        let fx = Fixture::new("status-clean");
        std::fs::write(fx.path().join("a.txt"), "one\n").unwrap();
        fx.git(&["add", "."]);
        fx.commit("first");

        let repo = Repo::open(fx.path()).unwrap();
        assert_eq!(repo.working_changes().unwrap(), 0);
    }

    #[test]
    fn a_modified_and_an_untracked_file_are_both_counted() {
        let fx = Fixture::new("status-dirty");
        std::fs::write(fx.path().join("a.txt"), "one\n").unwrap();
        fx.git(&["add", "."]);
        fx.commit("first");
        std::fs::write(fx.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(fx.path().join("new.txt"), "x\n").unwrap();

        let repo = Repo::open(fx.path()).unwrap();
        assert_eq!(repo.working_changes().unwrap(), 2);
    }

    #[test]
    fn an_unborn_repository_reports_zero_rather_than_failing() {
        let fx = Fixture::new("status-unborn");
        let repo = Repo::open(fx.path()).unwrap();
        assert_eq!(repo.working_changes().unwrap(), 0);
    }

    /// The test above is satisfied trivially by an empty working tree
    /// regardless of how an unborn repository is handled internally: it would
    /// pass even if `gix` errored on a missing HEAD and this function simply
    /// mapped that error to `Ok(0)` — or even if it never hit that code path
    /// at all. This test puts an untracked file in the unborn repository so
    /// the "no HEAD" status walk actually has something to report, which is
    /// what confirms `gix` handles it gracefully rather than the count being
    /// right for an unrelated reason.
    #[test]
    fn an_unborn_repository_with_an_untracked_file_is_still_counted() {
        let fx = Fixture::new("status-unborn-dirty");
        std::fs::write(fx.path().join("new.txt"), "x\n").unwrap();

        let repo = Repo::open(fx.path()).unwrap();
        assert_eq!(repo.working_changes().unwrap(), 1);
    }
}
