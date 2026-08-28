//! What a commit changed: which files, and by how many lines.
//!
//! Free of ratatui, like the rest of `git/`. Everything here can be slow, so
//! nothing here may be called from the render thread — the worker in
//! [`crate::worker`] owns that discipline.

use gix::diff::blob::platform::prepare_diff::Operation as PrepareOperation;
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

/// One rendered row of a file diff. Line numbers are 1-based, as a reader
/// expects, and are the numbers in the real files rather than hunk offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Context { old: u32, new: u32, text: String },
    Added { new: u32, text: String },
    Removed { old: u32, text: String },
}

/// One file's diff, ready to paint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub lines: Vec<DiffLine>,
    /// The content could not be diffed as text; `lines` is empty.
    pub binary: bool,
    /// The diff was longer than [`MAX_DIFF_LINES`] and was cut short.
    pub truncated: bool,
}

impl FileDiff {
    /// A diff of a blob that cannot be read as text.
    fn binary(path: &str) -> Self {
        Self {
            path: path.to_string(),
            lines: Vec::new(),
            binary: true,
            truncated: false,
        }
    }
}

/// A single file's diff is bounded so one enormous generated file cannot make
/// the pane's own state unbounded. The viewer says so when it bites.
pub const MAX_DIFF_LINES: usize = 20_000;

impl Repo {
    /// Diff one path in `id` against the same path in its first parent.
    ///
    /// `context` is the number of unchanged lines to keep on each side of a
    /// change. Cost is proportional to the file, so this belongs on the worker
    /// thread.
    pub fn file_diff(
        &self,
        id: gix::ObjectId,
        path: &str,
        context: u32,
    ) -> Result<FileDiff, ReadError> {
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
        // An external diff driver would hand back a command to run instead of
        // a diffable pair of buffers, and we have no line numbers without the
        // buffers. gix's own `line_counts` disables it the same way.
        cache.options.skip_internal_diff_if_external_is_configured = false;
        let mut found: Option<FileDiff> = None;

        old_tree
            .changes()
            .map_err(|e| ReadError::from_err("starting a tree diff", e))?
            .for_each_to_obtain_tree(
                &new_tree,
                |change| -> Result<Action, std::convert::Infallible> {
                    // The walk reports directory entries too, and every path in
                    // the commit; only the one blob we were asked about matters.
                    if found.is_some()
                        || !change.entry_mode().is_blob()
                        || change.location() != path
                    {
                        return Ok(Action::Continue(()));
                    }
                    let Ok(platform) = change.diff(&mut cache) else {
                        found = Some(FileDiff::binary(path));
                        return Ok(Action::Continue(()));
                    };
                    // `lines()` gives hunk contents but throws the line numbers
                    // away, so drop one level to reach hunks that carry ranges.
                    let Ok(prep) = platform.resource_cache.prepare_diff() else {
                        found = Some(FileDiff::binary(path));
                        return Ok(Action::Continue(()));
                    };
                    // A binary blob is not an error here: `prepare_diff`
                    // succeeds and says so through its operation.
                    if matches!(
                        prep.operation,
                        PrepareOperation::SourceOrDestinationIsBinary
                    ) {
                        found = Some(FileDiff::binary(path));
                        return Ok(Action::Continue(()));
                    }
                    let input = prep.interned_input();
                    let diff = gix::diff::blob::diff_with_slider_heuristics(
                        gix::diff::blob::Algorithm::Histogram,
                        &input,
                    );
                    found = Some(assemble(path, &input, diff.hunks(), context));
                    Ok(Action::Continue(()))
                },
            )
            .map_err(|e| ReadError::from_err("walking a tree diff", e))?;

        found.ok_or_else(|| ReadError(format!("{path} was not changed by this commit")))
    }
}

/// Turn hunks and the interned files into numbered rows with `context`
/// unchanged lines around each change.
fn assemble(
    path: &str,
    input: &gix::diff::blob::InternedInput<&[u8]>,
    hunks: impl Iterator<Item = gix::diff::blob::Hunk>,
    context: u32,
) -> FileDiff {
    let text = |token| String::from_utf8_lossy(input.interner[token]).into_owned();
    let mut lines: Vec<DiffLine> = Vec::new();
    let mut truncated = false;
    // The first line of `before`/`after` not yet emitted, 0-based.
    let (mut old_cursor, mut new_cursor) = (0u32, 0u32);

    for hunk in hunks {
        if lines.len() >= MAX_DIFF_LINES {
            truncated = true;
            break;
        }
        let lead = hunk.before.start.saturating_sub(context).max(old_cursor);
        // Leading context, taken from the unchanged side.
        for i in lead..hunk.before.start {
            let new = new_cursor + (i - old_cursor);
            lines.push(DiffLine::Context {
                old: i + 1,
                new: new + 1,
                text: text(input.before[i as usize]),
            });
        }
        // No cursor update here: the change itself consumes `before.start
        // ..before.end` and `after.start..after.end`, so both cursors land on
        // the hunk's ends below regardless of where they were.
        for i in hunk.before.clone() {
            lines.push(DiffLine::Removed {
                old: i + 1,
                text: text(input.before[i as usize]),
            });
        }
        for i in hunk.after.clone() {
            lines.push(DiffLine::Added {
                new: i + 1,
                text: text(input.after[i as usize]),
            });
        }
        old_cursor = hunk.before.end;
        new_cursor = hunk.after.end;

        // Trailing context.
        let tail_end = (hunk.before.end + context).min(input.before.len() as u32);
        for i in hunk.before.end..tail_end {
            let new = new_cursor + (i - old_cursor);
            lines.push(DiffLine::Context {
                old: i + 1,
                new: new + 1,
                text: text(input.before[i as usize]),
            });
        }
        new_cursor += tail_end - old_cursor;
        old_cursor = tail_end;
    }

    if lines.len() > MAX_DIFF_LINES {
        lines.truncate(MAX_DIFF_LINES);
        truncated = true;
    }
    FileDiff {
        path: path.to_string(),
        lines,
        binary: false,
        truncated,
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

    /// The rendered shape of a diff, one character per line kind, for compact
    /// assertions: `.` context, `+` added, `-` removed.
    fn shape(d: &FileDiff) -> String {
        d.lines
            .iter()
            .map(|l| match l {
                DiffLine::Context { .. } => '.',
                DiffLine::Added { .. } => '+',
                DiffLine::Removed { .. } => '-',
            })
            .collect()
    }

    #[test]
    fn a_modified_file_yields_numbered_lines_with_context() {
        let fx = Fixture::new("filediff-basic");
        write_commit(&fx, "a.txt", "1\n2\n3\n4\n5\n6\n7\n8\n9\n", "first");
        let id = write_commit(
            &fx,
            "a.txt",
            "1\n2\n3\n4\nFIVE\n6\n7\n8\n9\n",
            "change five",
        );

        let repo = Repo::open(fx.path()).unwrap();
        let d = repo.file_diff(id.parse().unwrap(), "a.txt", 2).unwrap();

        assert_eq!(d.path, "a.txt");
        assert!(!d.binary);
        // Two context lines each side of a one-line replacement.
        assert_eq!(shape(&d), "..-+..");

        // Numbers are the real file line numbers, 1-based.
        match &d.lines[0] {
            DiffLine::Context { old, new, text } => {
                assert_eq!((*old, *new), (3, 3));
                assert_eq!(text, "3");
            }
            other => panic!("expected context, got {other:?}"),
        }
        match &d.lines[2] {
            DiffLine::Removed { old, text } => {
                assert_eq!(*old, 5);
                assert_eq!(text, "5");
            }
            other => panic!("expected removal, got {other:?}"),
        }
        match &d.lines[3] {
            DiffLine::Added { new, text } => {
                assert_eq!(*new, 5);
                assert_eq!(text, "FIVE");
            }
            other => panic!("expected addition, got {other:?}"),
        }
    }

    #[test]
    fn context_zero_yields_only_changed_lines() {
        let fx = Fixture::new("filediff-nocontext");
        write_commit(&fx, "a.txt", "1\n2\n3\n", "first");
        let id = write_commit(&fx, "a.txt", "1\nTWO\n3\n", "change two");

        let repo = Repo::open(fx.path()).unwrap();
        let d = repo.file_diff(id.parse().unwrap(), "a.txt", 0).unwrap();
        assert_eq!(shape(&d), "-+");
    }

    #[test]
    fn a_file_absent_from_the_commit_is_an_error_not_an_empty_diff() {
        let fx = Fixture::new("filediff-missing");
        let id = write_commit(&fx, "a.txt", "1\n", "first");

        let repo = Repo::open(fx.path()).unwrap();
        let err = repo
            .file_diff(id.parse().unwrap(), "nope.txt", 3)
            .expect_err("a path the commit did not touch has no diff");
        assert!(err.to_string().contains("nope.txt"), "{err}");
    }

    #[test]
    fn a_binary_file_reports_binary_rather_than_lines() {
        let fx = Fixture::new("filediff-binary");
        write_commit(&fx, "a.txt", "1\n", "first");
        std::fs::write(fx.path().join("b.bin"), [0u8, 159, 146, 150, 0, 1, 2]).unwrap();
        fx.git(&["add", "b.bin"]);
        let id = fx.commit("add binary");

        let repo = Repo::open(fx.path()).unwrap();
        let d = repo.file_diff(id.parse().unwrap(), "b.bin", 3).unwrap();
        assert!(d.binary);
        assert!(d.lines.is_empty(), "{:?}", d.lines);
    }

    /// Two hunks in one file, with the first changing the file's length so the
    /// old and new numbering diverge. This is what the cursor arithmetic in
    /// `assemble` exists for, and the single-hunk tests above cannot catch a
    /// drift in it. Every row below matches `git diff -U2`, whose second hunk
    /// header for this input is `@@ -13,5 +11,5 @@`.
    #[test]
    fn numbering_stays_in_sync_across_hunks_that_change_the_length() {
        let before: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let after: String = (1..=20)
            .filter(|i| !matches!(i, 2 | 3))
            .map(|i| {
                if i == 15 {
                    "FIFTEEN\n".to_string()
                } else {
                    format!("{i}\n")
                }
            })
            .collect();

        let fx = Fixture::new("filediff-diverging");
        write_commit(&fx, "a.txt", &before, "first");
        let id = write_commit(&fx, "a.txt", &after, "drop two, change one");

        let repo = Repo::open(fx.path()).unwrap();
        let d = repo.file_diff(id.parse().unwrap(), "a.txt", 2).unwrap();

        assert_eq!(shape(&d), ".--....-+..");
        let numbered: Vec<(char, u32, u32)> = d
            .lines
            .iter()
            .map(|l| match l {
                DiffLine::Context { old, new, .. } => ('.', *old, *new),
                DiffLine::Added { new, .. } => ('+', 0, *new),
                DiffLine::Removed { old, .. } => ('-', *old, 0),
            })
            .collect();
        assert_eq!(
            numbered,
            [
                ('.', 1, 1),
                ('-', 2, 0),
                ('-', 3, 0),
                ('.', 4, 2),
                ('.', 5, 3),
                // The elided middle of the file: the next hunk resumes at the
                // real line numbers on both sides, not at a hunk offset.
                ('.', 13, 11),
                ('.', 14, 12),
                ('-', 15, 0),
                ('+', 0, 13),
                ('.', 16, 14),
                ('.', 17, 15),
            ]
        );
    }
}
