//! Read-only comparisons of HEAD, index, and raw working-tree content.
use std::io::Read;

use super::commit::ReadError;
use super::diff::{CommitDiff, DiffLine, FileChange, FileDiff, FileStat};
use super::repo::Repo;

const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
/// Before and after bytes; None denotes an absent path.
type FileSides = (Option<Vec<u8>>, Option<Vec<u8>>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorktreeStage {
    Staged,
    Unstaged,
    Untracked,
}
impl WorktreeStage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Unstaged => "unstaged",
            Self::Untracked => "untracked",
        }
    }
}

impl Repo {
    pub fn working_diff(&self) -> Result<CommitDiff, ReadError> {
        let status = self
            .gix()
            .status(gix::progress::Discard)
            .map_err(|e| ReadError::from_err("opening status", e))?
            .untracked_files(gix::status::UntrackedFiles::Files)
            .index_worktree_submodules(None)
            .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
            .index_worktree_rewrites(None)
            .into_iter(None)
            .map_err(|e| ReadError::from_err("reading status", e))?;
        let mut out = CommitDiff::default();
        for item in status {
            let item = item.map_err(|e| ReadError::from_err("reading status entry", e))?;
            let stage = match &item {
                gix::status::Item::TreeIndex(_) => WorktreeStage::Staged,
                gix::status::Item::IndexWorktree(
                    gix::status::index_worktree::Item::DirectoryContents { .. },
                ) => WorktreeStage::Untracked,
                _ => WorktreeStage::Unstaged,
            };
            let mut stat = file_stat(item.location(), stage);
            if stat.stage.is_none() {
                out.files.push(stat);
                continue;
            }
            let path = stat.path.clone();
            match self.working_sides(&path, stage) {
                Ok((before, after)) => {
                    stat.change = match (&before, &after) {
                        (None, _) => FileChange::Added,
                        (_, None) => FileChange::Deleted,
                        _ => FileChange::Modified,
                    };
                    let diff = compare(
                        &path,
                        before.as_deref().unwrap_or_default(),
                        after.as_deref().unwrap_or_default(),
                        0,
                    );
                    stat.binary = diff.binary;
                    stat.insertions = diff
                        .lines
                        .iter()
                        .filter(|line| matches!(line, DiffLine::Added { .. }))
                        .count() as u32;
                    stat.removals = diff
                        .lines
                        .iter()
                        .filter(|line| matches!(line, DiffLine::Removed { .. }))
                        .count() as u32;
                    if diff.truncated {
                        stat.unreadable = Some("line totals exceed display limit".into());
                    }
                    out.insertions += stat.insertions;
                    out.removals += stat.removals;
                }
                Err(error) => stat.unreadable = Some(error.to_string()),
            }
            out.files.push(stat);
        }
        out.files
            .sort_by(|a, b| a.stage.cmp(&b.stage).then(a.path.cmp(&b.path)));
        Ok(out)
    }

    pub fn working_file_diff(
        &self,
        path: &str,
        stage: WorktreeStage,
        context: u32,
    ) -> Result<FileDiff, ReadError> {
        let (before, after) = self.working_sides(path, stage)?;
        Ok(compare(
            path,
            before.as_deref().unwrap_or_default(),
            after.as_deref().unwrap_or_default(),
            context,
        ))
    }

    fn working_sides(&self, path: &str, stage: WorktreeStage) -> Result<FileSides, ReadError> {
        let index = self
            .gix()
            .index_or_empty()
            .map_err(|e| ReadError::from_err("reading index", e))?;
        let index_id = index
            .entry_by_path(path.as_bytes().into())
            .map(|entry| entry.id);
        let index_bytes = || index_id.map(|id| self.read_blob(id)).transpose();
        match stage {
            WorktreeStage::Staged => {
                let tree = self
                    .gix()
                    .head_tree_id_or_empty()
                    .map_err(|e| ReadError::from_err("reading HEAD", e))?
                    .object()
                    .map_err(|e| ReadError::from_err("reading HEAD tree", e))?
                    .try_into_tree()
                    .map_err(|e| ReadError::from_err("reading HEAD tree", e))?;
                let id = tree
                    .lookup_entry_by_path(path)
                    .map_err(|e| ReadError::from_err("reading HEAD path", e))?
                    .map(|entry| entry.object_id());
                Ok((id.map(|id| self.read_blob(id)).transpose()?, index_bytes()?))
            }
            WorktreeStage::Unstaged => Ok((index_bytes()?, self.read_worktree(path)?)),
            WorktreeStage::Untracked => Ok((None, self.read_worktree(path)?)),
        }
    }

    fn read_blob(&self, id: gix::ObjectId) -> Result<Vec<u8>, ReadError> {
        let header = self
            .gix()
            .find_header(id)
            .map_err(|e| ReadError::from_err("reading blob header", e))?;
        if header.size() > MAX_FILE_BYTES as u64 {
            return Err(ReadError("file exceeds 8 MiB review limit".into()));
        }
        let blob = self
            .gix()
            .find_blob(id)
            .map_err(|e| ReadError::from_err("reading blob", e))?;
        if blob.data.len() > MAX_FILE_BYTES {
            return Err(ReadError("file exceeds 8 MiB review limit".into()));
        }
        Ok(blob.data.clone())
    }

    fn read_worktree(&self, path: &str) -> Result<Option<Vec<u8>>, ReadError> {
        let relative = std::path::Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(ReadError("invalid repository-relative path".into()));
        }
        let full = self.workdir().join(relative);
        let metadata = match std::fs::symlink_metadata(&full) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(ReadError::from_err("reading working file", e)),
        };
        let parent = full
            .parent()
            .ok_or_else(|| ReadError("missing parent".into()))?
            .canonicalize()
            .map_err(|e| ReadError::from_err("resolving working directory", e))?;
        let root = self
            .workdir()
            .canonicalize()
            .map_err(|e| ReadError::from_err("resolving repository", e))?;
        if !parent.starts_with(root) {
            return Err(ReadError("working path leaves repository".into()));
        }
        if metadata.file_type().is_symlink() {
            return std::fs::read_link(full)
                .map(|target| Some(gix::path::into_bstr(target).into_owned().into()))
                .map_err(|e| ReadError::from_err("reading symbolic link", e));
        }
        if !metadata.is_file() {
            return Err(ReadError("non-file entries cannot be diffed".into()));
        }
        let mut bytes = Vec::new();
        std::fs::File::open(full)
            .and_then(|file| {
                file.take((MAX_FILE_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
            })
            .map_err(|e| ReadError::from_err("reading working file", e))?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(ReadError("file exceeds 8 MiB review limit".into()));
        }
        Ok(Some(bytes))
    }
}

fn compare(path: &str, before: &[u8], after: &[u8], context: u32) -> FileDiff {
    if before.contains(&0) || after.contains(&0) {
        return FileDiff::binary(path);
    }
    use gix::diff::blob::platform::resource::ByteLinesWithoutTerminator;
    let input = gix::diff::blob::InternedInput::new(
        ByteLinesWithoutTerminator::new(before),
        ByteLinesWithoutTerminator::new(after),
    );
    let diff =
        gix::diff::blob::diff_with_slider_heuristics(gix::diff::blob::Algorithm::Histogram, &input);
    super::diff::assemble(path, &input, diff.hunks(), context)
}

#[cfg(test)]
mod tests;

fn file_stat(path: &gix::bstr::BStr, stage: WorktreeStage) -> FileStat {
    let decoded = std::str::from_utf8(path.as_ref());
    let invalid = decoded.is_err();
    FileStat {
        path: decoded
            .map(str::to_owned)
            .unwrap_or_else(|_| format!("{path:?}")),
        change: FileChange::Modified,
        stage: (!invalid).then_some(stage),
        insertions: 0,
        removals: 0,
        binary: false,
        unreadable: invalid.then(|| "filename is not UTF-8; cannot safely open this path".into()),
    }
}
