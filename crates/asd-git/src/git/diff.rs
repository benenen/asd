//! What a commit changed: which files, and by how many lines.
//!
//! Free of ratatui, like the rest of `git/`. Everything here can be slow, so
//! nothing here may be called from the render thread — the worker in
//! [`crate::worker`] owns that discipline.

use gix::object::tree::diff::Action;

use crate::git::commit::ReadError;
use crate::git::repo::Repo;

/// How a file entered this commit's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Added,
    Modified,
    Deleted,
    /// gix reports a rewrite; `from` is the path it came from.
    Renamed {
        from: String,
    },
}

/// One row of the changed-files pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub path: String,
    pub change: FileChange,
    /// Lines added. Always 0 for a binary file.
    pub insertions: u32,
    /// Lines removed. Always 0 for a binary file.
    pub removals: u32,
    /// True when the content could not be diffed as text.
    pub binary: bool,
}

/// Everything the detail and file panes need for one commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitDiff {
    pub files: Vec<FileStat>,
    /// Totals across every text file, for the "N files changed" header.
    pub insertions: u32,
    pub removals: u32,
}

impl Repo {
    /// Diff `id` against its first parent, or against the empty tree when it is
    /// a root commit.
    ///
    /// Cost is proportional to the commit's size, so this belongs on the worker
    /// thread and never on the thread that paints the UI.
    pub fn commit_diff(&self, id: gix::ObjectId) -> Result<CommitDiff, ReadError> {
        let repo = self.gix();
        let commit = repo
            .find_commit(id)
            .map_err(|e| ReadError::from_err("finding a commit", e))?;
        let new_tree = commit
            .tree()
            .map_err(|e| ReadError::from_err("reading a commit tree", e))?;
        let old_tree = match commit.parent_ids().next() {
            Some(parent) => {
                let parent_commit = repo
                    .find_commit(parent)
                    .map_err(|e| ReadError::from_err("finding the parent commit", e))?;
                parent_commit
                    .tree()
                    .map_err(|e| ReadError::from_err("reading the parent tree", e))?
            }
            None => repo.empty_tree(),
        };

        let mut cache = repo
            .diff_resource_cache_for_tree_diff()
            .map_err(|e| ReadError::from_err("preparing a diff cache", e))?;
        let mut out = CommitDiff::default();

        old_tree
            .changes()
            .map_err(|e| ReadError::from_err("starting a tree diff", e))?
            .for_each_to_obtain_tree(
                &new_tree,
                |change| -> Result<Action, std::convert::Infallible> {
                    // The walk reports tree entries as well as blobs; only blobs
                    // are files. Without this the "N files changed" count silently
                    // includes every directory above a nested change.
                    if !change.entry_mode().is_blob() {
                        return Ok(Action::Continue(()));
                    }
                    let path = change.location().to_string();
                    let kind = match &change {
                        gix::object::tree::diff::Change::Addition { .. } => FileChange::Added,
                        gix::object::tree::diff::Change::Deletion { .. } => FileChange::Deleted,
                        gix::object::tree::diff::Change::Modification { .. } => {
                            FileChange::Modified
                        }
                        gix::object::tree::diff::Change::Rewrite {
                            source_location, ..
                        } => FileChange::Renamed {
                            from: source_location.to_string(),
                        },
                    };

                    let mut stat = FileStat {
                        path,
                        change: kind,
                        insertions: 0,
                        removals: 0,
                        binary: false,
                    };
                    // A file whose blob cannot be diffed is listed without counts
                    // rather than failing the whole commit. gix's diff-preparation
                    // and line-counting steps use distinct error types with no
                    // `From` conversion between them, so this is two nested
                    // `if let`s rather than one chained `Result`; either failure
                    // means the same thing here: treat the file as binary.
                    if let Ok(mut prepared) = change.diff(&mut cache) {
                        if let Ok(Some(counts)) = prepared.line_counts() {
                            stat.insertions = counts.insertions;
                            stat.removals = counts.removals;
                            out.insertions += counts.insertions;
                            out.removals += counts.removals;
                        } else {
                            stat.binary = true;
                        }
                    } else {
                        stat.binary = true;
                    }
                    out.files.push(stat);
                    Ok(Action::Continue(()))
                },
            )
            .map_err(|e| ReadError::from_err("walking a tree diff", e))?;

        out.files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::Fixture;
    use crate::git::repo::Repo;

    /// Write `body` to `name`, stage it, and commit.
    fn write_commit(fx: &Fixture, name: &str, body: &str, summary: &str) -> String {
        std::fs::write(fx.path().join(name), body).unwrap();
        fx.git(&["add", name]);
        fx.commit(summary)
    }

    #[test]
    fn a_first_commit_reports_every_file_as_added() {
        let fx = Fixture::new("diff-first");
        let id = write_commit(&fx, "a.txt", "one\ntwo\n", "first");

        let repo = Repo::open(fx.path()).unwrap();
        let d = repo.commit_diff(id.parse().unwrap()).unwrap();

        assert_eq!(d.files.len(), 1, "{:?}", d.files);
        assert_eq!(d.files[0].path, "a.txt");
        assert_eq!(d.files[0].change, FileChange::Added);
        assert_eq!(d.files[0].insertions, 2);
        assert_eq!(d.files[0].removals, 0);
        assert_eq!((d.insertions, d.removals), (2, 0));
    }

    #[test]
    fn a_modification_counts_both_directions() {
        let fx = Fixture::new("diff-modify");
        write_commit(&fx, "a.txt", "one\ntwo\nthree\n", "first");
        let id = write_commit(&fx, "a.txt", "one\nTWO\nthree\nfour\n", "second");

        let repo = Repo::open(fx.path()).unwrap();
        let d = repo.commit_diff(id.parse().unwrap()).unwrap();

        assert_eq!(d.files.len(), 1);
        assert_eq!(d.files[0].change, FileChange::Modified);
        // one line replaced (+1/-1) and one appended (+1)
        assert_eq!((d.files[0].insertions, d.files[0].removals), (2, 1));
    }

    #[test]
    fn a_deletion_is_reported_as_deleted() {
        let fx = Fixture::new("diff-delete");
        write_commit(&fx, "a.txt", "one\n", "first");
        std::fs::remove_file(fx.path().join("a.txt")).unwrap();
        fx.git(&["rm", "--quiet", "a.txt"]);
        let id = fx.commit("drop a");

        let repo = Repo::open(fx.path()).unwrap();
        let d = repo.commit_diff(id.parse().unwrap()).unwrap();
        assert_eq!(d.files.len(), 1);
        assert_eq!(d.files[0].change, FileChange::Deleted);
        assert_eq!((d.files[0].insertions, d.files[0].removals), (0, 1));
    }

    #[test]
    fn directories_are_not_counted_as_files() {
        // The tree walk reports directory entries too. A commit that only
        // touches a nested file must report exactly one file, not the file
        // plus every directory above it.
        let fx = Fixture::new("diff-nested");
        write_commit(&fx, "a.txt", "one\n", "first");
        std::fs::create_dir_all(fx.path().join("deep/er")).unwrap();
        let id = write_commit(&fx, "deep/er/b.txt", "x\n", "nested");

        let repo = Repo::open(fx.path()).unwrap();
        let d = repo.commit_diff(id.parse().unwrap()).unwrap();
        let paths: Vec<_> = d.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            ["deep/er/b.txt"],
            "directories leaked into the file list"
        );
    }

    #[test]
    fn a_binary_file_is_flagged_rather_than_counted() {
        let fx = Fixture::new("diff-binary");
        write_commit(&fx, "a.txt", "one\n", "first");
        std::fs::write(fx.path().join("b.bin"), [0u8, 159, 146, 150, 0, 1, 2]).unwrap();
        fx.git(&["add", "b.bin"]);
        let id = fx.commit("add binary");

        let repo = Repo::open(fx.path()).unwrap();
        let d = repo.commit_diff(id.parse().unwrap()).unwrap();
        let bin = d
            .files
            .iter()
            .find(|f| f.path == "b.bin")
            .expect("binary listed");
        assert!(bin.binary, "binary file must be flagged");
        assert_eq!(
            (bin.insertions, bin.removals),
            (0, 0),
            "no line counts for a binary"
        );
    }
}
