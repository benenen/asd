//! Bounded, read-only workspace enumeration in the daemon filesystem.
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use asd_proto::{Frame, SessionIdentity, WorkspaceEntry, WorkspaceEntryKind};

const ENTRY_LIMIT: usize = 2000;
const TEXT_LIMIT: usize = 512 * 1024;
const PATH_LIMIT: usize = 8192;
const TIME_LIMIT: Duration = Duration::from_secs(5);
static READERS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(4)));

pub(crate) async fn collect(
    identity: SessionIdentity,
    directory: PathBuf,
    path: String,
) -> Result<Frame, String> {
    let permit = Arc::clone(&READERS)
        .try_acquire_owned()
        .map_err(|_| "workspace listing is busy; try again".to_string())?;
    let work = tokio::task::spawn_blocking(move || {
        // Keep the permit until filesystem calls actually finish, even after timeout.
        let _permit = permit;
        collect_blocking(identity, directory, path)
    });
    tokio::time::timeout(TIME_LIMIT, work)
        .await
        .map_err(|_| "workspace listing timed out".to_string())?
        .map_err(|error| format!("workspace listing failed: {error}"))?
}

fn resolve(root: &Path, path: &str) -> Result<PathBuf, String> {
    if path.len() > PATH_LIMIT {
        return Err("workspace path exceeds listing limit".into());
    }
    let mut target = root.to_path_buf();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(name) => target.push(name),
            Component::CurDir => continue,
            _ => return Err("workspace path must be relative without parent traversal".into()),
        }
        let metadata = std::fs::symlink_metadata(&target)
            .map_err(|error| format!("cannot inspect workspace directory: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err("workspace navigation through symbolic links is disabled".into());
        }
    }
    let target = target
        .canonicalize()
        .map_err(|error| format!("cannot resolve workspace directory: {error}"))?;
    if !target.starts_with(root) {
        return Err("workspace directory escapes its root".into());
    }
    Ok(target)
}

fn collect_blocking(
    identity: SessionIdentity,
    directory: PathBuf,
    path: String,
) -> Result<Frame, String> {
    let started = Instant::now();
    let root = directory
        .canonicalize()
        .map_err(|error| format!("cannot resolve workspace root: {error}"))?;
    let target = resolve(&root, &path)?;
    let root_text = root.to_str().ok_or("workspace root is not valid UTF-8")?;
    let relative = target
        .strip_prefix(&root)
        .map_err(|_| "workspace directory escapes its root")?;
    // Wire paths use slash separators on every host; entry names remain exact.
    let path = relative
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .ok_or("workspace path is not valid UTF-8")
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("/");
    if root_text.len() > PATH_LIMIT || path.len() > PATH_LIMIT {
        return Err("workspace path exceeds listing limit".into());
    }
    let reader = std::fs::read_dir(&target)
        .map_err(|error| format!("cannot list workspace directory: {error}"))?;
    let mut entries = Vec::new();
    let mut text_size = 0;
    let mut truncated = false;
    for (index, entry) in reader.enumerate() {
        if index == ENTRY_LIMIT || started.elapsed() >= TIME_LIMIT {
            truncated = true;
            break;
        }
        let entry = entry.map_err(|error| format!("cannot read workspace entry: {error}"))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "workspace entry name is not valid UTF-8")?;
        text_size += name.len();
        if text_size > TEXT_LIMIT {
            truncated = true;
            break;
        }
        let kind = entry
            .file_type()
            .map_err(|error| format!("cannot inspect workspace entry: {error}"))?;
        let kind = if kind.is_symlink() {
            WorkspaceEntryKind::Symlink
        } else if kind.is_dir() {
            WorkspaceEntryKind::Directory
        } else if kind.is_file() {
            WorkspaceEntryKind::File
        } else {
            WorkspaceEntryKind::Other
        };
        entries.push(WorkspaceEntry { name, kind });
    }
    entries.sort_by(|a, b| {
        (a.kind != WorkspaceEntryKind::Directory, &a.name)
            .cmp(&(b.kind != WorkspaceEntryKind::Directory, &b.name))
    });
    Ok(Frame::WorkspaceFiles {
        identity,
        root: root_text.to_owned(),
        path,
        entries,
        truncated,
    })
}

#[cfg(test)]
mod tests;
