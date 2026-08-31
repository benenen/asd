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
    /// Why this file's blobs could not be read, when they could not be.
    ///
    /// A soft failure: the counts are unknown and stay 0, the row is marked,
    /// and the rest of the commit is listed as usual. One corrupt object
    /// blanking both the detail and changed-files panes would say less about
    /// the commit than listing every file but one — the same reading the
    /// overlay already takes of an unreadable *commit*, which is surfaced
    /// beside the history rather than in place of it.
    pub unreadable: Option<String>,
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
    ///
    /// A file whose blobs cannot be read is marked
    /// ([`FileStat::unreadable`]) rather than failing the call: the other
    /// files in the commit are still worth showing.
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

        let walk = old_tree
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
                        unreadable: None,
                    };
                    // `line_counts` returns `Ok(None)` for a binary blob and
                    // only for that, so binary is the `Ok(None)` arm alone. A
                    // real failure — an unreadable object, an unset resource —
                    // marks the row instead, rather than being listed as
                    // binary (which would be a wrong answer) or aborting the
                    // walk (which would hide every other file in the commit).
                    match change.diff(&mut cache) {
                        Ok(mut prepared) => match prepared.line_counts() {
                            Ok(Some(counts)) => {
                                stat.insertions = counts.insertions;
                                stat.removals = counts.removals;
                                out.insertions += counts.insertions;
                                out.removals += counts.removals;
                            }
                            Ok(None) => stat.binary = true,
                            Err(e) => {
                                stat.unreadable = Some(
                                    ReadError::from_err("counting a file's changed lines", e).0,
                                );
                            }
                        },
                        Err(e) => {
                            stat.unreadable =
                                Some(ReadError::from_err("reading a file's blobs", e).0);
                        }
                    }
                    out.files.push(stat);
                    Ok(Action::Continue(()))
                },
            );

        // Nothing in the callback breaks the walk any more, so its only error
        // is a genuine failure to walk the trees — which is about the commit
        // rather than about one file, and so is fatal to this call.
        walk.map_err(|e| ReadError::from_err("walking a tree diff", e))?;

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
        // Raised after the walk: the callback cannot return an error without
        // gix flattening it to "The user-provided callback failed", which would
        // throw away the message the user is meant to read.
        let mut failure: Option<ReadError> = None;

        let walk = old_tree
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
                    // Only a binary blob is reported as binary. An unreadable
                    // object or an unset resource is a genuine failure and must
                    // not masquerade as "this file has no text".
                    let platform = match change.diff(&mut cache) {
                        Ok(platform) => platform,
                        Err(e) => {
                            failure = Some(ReadError::from_err("reading a file's blobs", e));
                            return Ok(Action::Break(()));
                        }
                    };
                    // `lines()` gives hunk contents but throws the line numbers
                    // away, so drop one level to reach hunks that carry ranges.
                    let prep = match platform.resource_cache.prepare_diff() {
                        Ok(prep) => prep,
                        Err(e) => {
                            failure = Some(ReadError::from_err("preparing a file diff", e));
                            return Ok(Action::Break(()));
                        }
                    };
                    // `prepare_diff` reports both the binary case and the
                    // algorithm to use, and both have to be read from here.
                    //
                    // A binary blob is not an error: `prepare_diff` succeeds
                    // and says so through its operation.
                    //
                    // The algorithm matters just as much. `commit_diff` takes
                    // its `+N -M` from `line_counts`, which reads the algorithm
                    // out of this same field — the repository's own
                    // `diff.algorithm`, defaulting to Myers. Naming a different
                    // one here made the changed-files pane and this viewer
                    // answer the same question two ways: a file could read
                    // `+36 -28` in the pane and show 68 added lines once opened.
                    let algorithm = match prep.operation {
                        PrepareOperation::InternalDiff { algorithm } => algorithm,
                        PrepareOperation::SourceOrDestinationIsBinary => {
                            found = Some(FileDiff::binary(path));
                            return Ok(Action::Continue(()));
                        }
                        // Disabled above, so gix never chooses it. Reported
                        // rather than assumed away: nothing in this crate
                        // panics, and a wrong assumption here would.
                        PrepareOperation::ExternalCommand { .. } => {
                            failure = Some(ReadError(
                                "an external diff driver cannot be rendered with line numbers"
                                    .to_string(),
                            ));
                            return Ok(Action::Break(()));
                        }
                    };
                    let input = prep.interned_input();
                    // Slider heuristics move a hunk's boundary within a run of
                    // equal lines; they change where a change is shown, never
                    // how much of one there is. The totals still match
                    // `line_counts`, which does not apply them.
                    let diff = gix::diff::blob::diff_with_slider_heuristics(algorithm, &input);
                    found = Some(assemble(path, &input, diff.hunks(), context));
                    Ok(Action::Continue(()))
                },
            );

        // See `commit_diff`: a break is reported as a cancelled walk, so the
        // real cause must be raised ahead of it.
        if let Some(e) = failure {
            return Err(e);
        }
        walk.map_err(|e| ReadError::from_err("walking a tree diff", e))?;

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
    // Peekable because the trailing context below has to stop where the next
    // hunk's own lines begin.
    let mut hunks = hunks.peekable();
    let text = |token| String::from_utf8_lossy(input.interner[token]).into_owned();
    let mut lines: Vec<DiffLine> = Vec::new();
    let mut truncated = false;
    // The first line of `before`/`after` not yet emitted, 0-based.
    let (mut old_cursor, mut new_cursor) = (0u32, 0u32);

    while let Some(hunk) = hunks.next() {
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

        // Trailing context. `HunkIter` splits on a single unchanged line, so
        // two changes `context` or fewer lines apart are two hunks whose
        // context regions overlap. Without the clamp to the next hunk's first
        // line, this loop emits that line as unchanged — with its stale
        // pre-change text — and the next hunk then emits it again as a removal.
        // A `context` wide enough to span a whole later hunk would replay a
        // whole block and walk the line numbers backwards. git stops the same
        // way, as does imara's own unified-diff printer.
        let mut tail_end = (hunk.before.end + context).min(input.before.len() as u32);
        if let Some(next) = hunks.peek() {
            tail_end = tail_end.min(next.before.start);
        }
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

    /// Render a `FileDiff` the way `git diff` renders its hunk bodies, so the
    /// two can be compared directly rather than against a hand-written guess.
    fn rendered(d: &FileDiff) -> Vec<String> {
        d.lines
            .iter()
            .map(|l| match l {
                DiffLine::Context { text, .. } => format!(" {text}"),
                DiffLine::Added { text, .. } => format!("+{text}"),
                DiffLine::Removed { text, .. } => format!("-{text}"),
            })
            .collect()
    }

    /// The hunk bodies of `git diff -U{context}` across the last commit, with
    /// the headers dropped: the same shape `rendered` produces.
    ///
    /// `git_raw` rather than `git`: a blank context line is a lone space, and
    /// the trimmed form would drop it off either end of the output while
    /// `rendered` keeps it, turning a real disagreement into a pass.
    fn git_rendered(fx: &Fixture, path: &str, context: u32) -> Vec<String> {
        let out = fx.git_raw(&[
            "diff",
            &format!("-U{context}"),
            "HEAD~1",
            "HEAD",
            "--",
            path,
        ]);
        out.lines()
            .skip_while(|l| !l.starts_with("@@"))
            .filter(|l| !l.starts_with("@@"))
            .map(str::to_string)
            .collect()
    }

    /// `HunkIter` splits hunks on a single unchanged line, so two changes that
    /// sit within `context` of each other produce two hunks whose context
    /// regions overlap. The trailing context of the first must stop where the
    /// second begins, or the shared line is emitted twice — once as unchanged,
    /// carrying its stale pre-change text.
    ///
    /// The expectation here is real `git diff -Un` output read back from the
    /// fixture, not a written-down guess. Twelve distinct lines have one
    /// minimal alignment, so this fixture would agree with git whatever
    /// algorithm either side used; the repetitive-file oracle below is what
    /// pins the algorithm itself.
    #[test]
    fn hunks_close_together_render_exactly_like_git() {
        // (description, changed 1-based lines, context)
        let cases: [(&str, &[usize], u32); 4] = [
            ("one unchanged line between two hunks", &[3, 5], 2),
            ("exactly `context` unchanged lines between", &[3, 6], 2),
            ("context wide enough to span the later hunk", &[3, 5], 5),
            ("three changes in a row", &[3, 5, 7], 2),
        ];
        for (what, changed, context) in cases {
            let before: String = (1..=12).map(|i| format!("{i}\n")).collect();
            let after: String = (1..=12)
                .map(|i| {
                    if changed.contains(&i) {
                        format!("X{i}\n")
                    } else {
                        format!("{i}\n")
                    }
                })
                .collect();

            let fx = Fixture::new("filediff-close");
            write_commit(&fx, "a.txt", &before, "first");
            let id = write_commit(&fx, "a.txt", &after, "edits");

            let repo = Repo::open(fx.path()).unwrap();
            let d = repo
                .file_diff(id.parse().unwrap(), "a.txt", context)
                .unwrap();

            assert_eq!(
                rendered(&d),
                git_rendered(&fx, "a.txt", context),
                "{what}: shape {} does not match git",
                shape(&d)
            );

            // A duplicated line also walks the numbering backwards, so pin
            // that separately: each side's numbers strictly increase.
            let (mut last_old, mut last_new) = (0u32, 0u32);
            for line in &d.lines {
                let (old, new) = match line {
                    DiffLine::Context { old, new, .. } => (Some(*old), Some(*new)),
                    DiffLine::Added { new, .. } => (None, Some(*new)),
                    DiffLine::Removed { old, .. } => (Some(*old), None),
                };
                if let Some(old) = old {
                    assert!(old > last_old, "{what}: old went {last_old} -> {old}");
                    last_old = old;
                }
                if let Some(new) = new {
                    assert!(new > last_new, "{what}: new went {last_new} -> {new}");
                    last_new = new;
                }
            }
        }
    }

    /// A deterministic file of repetitive, source-like lines.
    ///
    /// The token set is what real source repeats: a closing brace, a blank
    /// line, an early return. Distinct lines have exactly one minimal
    /// alignment, so every diff algorithm agrees on them; repeated lines are
    /// where they part company.
    fn repetitive(seed: u64, lines: usize) -> Vec<String> {
        const TOKENS: [&str; 6] = [
            "}",
            "",
            "    return;",
            "    if (x) {",
            "        y += 1;",
            "    // step",
        ];
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as usize
        };
        (0..lines)
            .map(|_| TOKENS[next() % TOKENS.len()].to_string())
            .collect()
    }

    /// Delete every third-ish line and insert a marker in its place, so the
    /// two sides differ everywhere rather than in one block.
    fn edited(seed: u64, before: &[String]) -> Vec<String> {
        let mut state = seed.wrapping_mul(0xD1B5_4A32_D192_ED03).wrapping_add(7);
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as usize
        };
        let mut out = Vec::with_capacity(before.len());
        for line in before {
            match next() % 10 {
                0 => {}                                       // deleted
                1 => out.push("        z -= 1;".to_string()), // replaced
                2 => {
                    out.push(line.clone());
                    out.push("    // inserted".to_string());
                }
                _ => out.push(line.clone()),
            }
        }
        out
    }

    /// The oracle above only proves anything for the fixture it runs on, and
    /// twelve distinct lines is the fixture every diff algorithm agrees about.
    ///
    /// This one runs the same comparison over files that repeat themselves,
    /// which is where Myers and Histogram produce genuinely different — both
    /// valid, differently sized — diffs. It fails against a `file_diff` that
    /// names its own algorithm instead of taking the repository's, which is
    /// also the state in which the changed-files pane's `+N -M` (counted by
    /// gix with `diff.algorithm`) disagrees with what this viewer draws.
    #[test]
    fn a_repetitive_file_renders_exactly_like_git() {
        for seed in 1..=4u64 {
            let before = repetitive(seed, 300);
            let after = edited(seed, &before);
            let before: String = before.iter().map(|l| format!("{l}\n")).collect();
            let after: String = after.iter().map(|l| format!("{l}\n")).collect();

            let fx = Fixture::new("filediff-repetitive");
            write_commit(&fx, "a.txt", &before, "first");
            let id = write_commit(&fx, "a.txt", &after, "edits");

            let repo = Repo::open(fx.path()).unwrap();
            let d = repo.file_diff(id.parse().unwrap(), "a.txt", 3).unwrap();

            assert_eq!(
                rendered(&d),
                git_rendered(&fx, "a.txt", 3),
                "seed {seed}: our diff of a repetitive file is not git's"
            );
        }
    }

    /// The two halves of one answer: the count the changed-files pane shows
    /// and the lines the viewer draws must be the same diff.
    ///
    /// `commit_diff` reads the algorithm out of gix's configuration and
    /// `file_diff` used to hardcode Histogram, so on a repetitive file the
    /// pane said `+36 -28` and the viewer then painted 68 additions.
    #[test]
    fn the_pane_count_and_the_rendered_diff_agree() {
        for seed in 1..=4u64 {
            let before = repetitive(seed, 300);
            let after = edited(seed, &before);
            let before: String = before.iter().map(|l| format!("{l}\n")).collect();
            let after: String = after.iter().map(|l| format!("{l}\n")).collect();

            let fx = Fixture::new("filediff-agree");
            write_commit(&fx, "a.txt", &before, "first");
            let id = write_commit(&fx, "a.txt", &after, "edits");

            let repo = Repo::open(fx.path()).unwrap();
            let id: gix::ObjectId = id.parse().unwrap();
            let stat = repo.commit_diff(id).unwrap();
            let stat = stat.files.iter().find(|f| f.path == "a.txt").unwrap();
            let d = repo.file_diff(id, "a.txt", 3).unwrap();
            assert!(!d.truncated, "seed {seed}: the fixture must fit");

            let added = d
                .lines
                .iter()
                .filter(|l| matches!(l, DiffLine::Added { .. }))
                .count();
            let removed = d
                .lines
                .iter()
                .filter(|l| matches!(l, DiffLine::Removed { .. }))
                .count();
            assert_eq!(
                (added, removed),
                (stat.insertions as usize, stat.removals as usize),
                "seed {seed}: the pane counts and the viewer's lines disagree"
            );
        }
    }

    /// Corrupt the loose object holding `path` at HEAD.
    fn corrupt_head_blob(fx: &Fixture, path: &str) {
        let blob = fx.git(&["rev-parse", &format!("HEAD:{path}")]);
        let obj = fx
            .path()
            .join(".git/objects")
            .join(&blob[..2])
            .join(&blob[2..]);
        assert!(obj.exists(), "{} should be a loose object", obj.display());
        // Git stores loose objects read-only. Replace the directory entry so
        // this fixture also works for unprivileged CI users without chmod.
        std::fs::remove_file(&obj).unwrap();
        std::fs::write(&obj, b"not a git object at all").unwrap();
    }

    /// A blob that cannot be read is a failure, not a binary file. Both are
    /// reported through the same gix call sites, and conflating them turns a
    /// corrupt repository into a diff that quietly renders as "binary".
    #[test]
    fn an_unreadable_blob_is_an_error_rather_than_a_binary_file() {
        let fx = Fixture::new("diff-corrupt");
        write_commit(&fx, "a.txt", "1\n2\n3\n", "first");
        let id = write_commit(&fx, "a.txt", "1\nTWO\n3\n", "change two");
        corrupt_head_blob(&fx, "a.txt");

        let repo = Repo::open(fx.path()).unwrap();
        let id: gix::ObjectId = id.parse().unwrap();

        // `file_diff` was asked for this one file and has nothing else to
        // give back, so it fails — and says why.
        let Err(err) = repo.file_diff(id, "a.txt", 2) else {
            panic!("a corrupt blob must not read as a binary file");
        };
        let msg = err.to_string();
        // The cause has to survive: gix reports the callback's break as a
        // bare cancellation, which on its own says nothing useful.
        assert!(
            msg.contains("loose object"),
            "the real cause was swallowed: {msg}"
        );
        assert!(
            !msg.contains("cancelled"),
            "a cancellation leaked out instead of the cause: {msg}"
        );

        // `commit_diff` marks the file instead. Not binary: that would be a
        // wrong answer rather than a missing one.
        let d = repo.commit_diff(id).expect("the commit is still readable");
        let stat = d.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert!(!stat.binary, "a corrupt blob is not a binary file");
        let why = stat
            .unreadable
            .as_deref()
            .expect("the row is marked unreadable");
        assert!(
            why.contains("loose object"),
            "the cause was swallowed: {why}"
        );
    }

    /// One corrupt object must not blank the whole changed-files pane. The
    /// commit's other files are exactly what the reader still needs, and the
    /// overlay already takes that reading one layer up for an unreadable
    /// *commit*.
    #[test]
    fn one_unreadable_file_does_not_hide_the_rest_of_the_commit() {
        let fx = Fixture::new("diff-corrupt-partial");
        std::fs::write(fx.path().join("a.txt"), "1\n2\n3\n").unwrap();
        std::fs::write(fx.path().join("b.txt"), "x\n").unwrap();
        fx.git(&["add", "."]);
        fx.commit("first");
        std::fs::write(fx.path().join("a.txt"), "1\nTWO\n3\n").unwrap();
        std::fs::write(fx.path().join("b.txt"), "x\ny\n").unwrap();
        fx.git(&["add", "."]);
        let id = fx.commit("change both");
        corrupt_head_blob(&fx, "a.txt");

        let repo = Repo::open(fx.path()).unwrap();
        let d = repo
            .commit_diff(id.parse().unwrap())
            .expect("one bad blob must not fail the commit");

        let paths: Vec<&str> = d.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "b.txt"], "both files are still listed");
        assert!(d.files[0].unreadable.is_some(), "the bad one is marked");
        assert_eq!(
            (
                d.files[1].insertions,
                d.files[1].removals,
                &d.files[1].unreadable
            ),
            (1, 0, &None),
            "the good one still carries real counts"
        );
        // The header totals are what could be counted, not zero.
        assert_eq!((d.insertions, d.removals), (1, 0));
    }
}
