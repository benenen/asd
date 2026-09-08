use super::*;
use asd_proto::WorkspaceEntryKind;

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("asd-files-{}-{unique}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    async fn list(&self, path: &str) -> Result<Frame, String> {
        collect(
            SessionIdentity { instance_id: 7 },
            self.0.clone(),
            path.into(),
        )
        .await
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn workspace_lists_hidden_files_and_directories_without_git() {
    let dir = Directory::new();
    std::fs::write(dir.0.join(".hidden"), "").unwrap();
    std::fs::create_dir(dir.0.join("nested")).unwrap();
    let Frame::WorkspaceFiles {
        identity,
        root,
        path,
        entries,
        truncated,
    } = dir.list("").await.unwrap()
    else {
        panic!("expected listing");
    };
    assert_eq!(identity.instance_id, 7);
    assert_eq!(PathBuf::from(root), dir.0.canonicalize().unwrap());
    assert_eq!(path, "");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "nested");
    assert_eq!(entries[0].kind, WorkspaceEntryKind::Directory);
    assert_eq!(entries[1].name, ".hidden");
    assert_eq!(entries[1].kind, WorkspaceEntryKind::File);
    assert!(!truncated);
    let Frame::WorkspaceFiles { path, entries, .. } = dir.list("nested").await.unwrap() else {
        panic!("expected listing");
    };
    assert_eq!(path, "nested");
    assert!(entries.is_empty());
}

#[tokio::test]
async fn workspace_rejects_escape_missing_and_regular_file_paths() {
    let dir = Directory::new();
    std::fs::write(dir.0.join("file"), "").unwrap();
    for path in ["../", "nested/../", "/", "missing", "file"] {
        assert!(dir.list(path).await.is_err(), "accepted {path}");
    }
}

#[tokio::test]
async fn workspace_caps_entries_and_marks_omissions() {
    let dir = Directory::new();
    for i in 0..2001 {
        std::fs::write(dir.0.join(format!("file-{i}")), "").unwrap();
    }
    let Frame::WorkspaceFiles {
        entries, truncated, ..
    } = dir.list("").await.unwrap()
    else {
        panic!("expected listing");
    };
    assert_eq!(entries.len(), 2000);
    assert!(truncated);
    assert!(entries.windows(2).all(|pair| pair[0].name < pair[1].name));
}

#[tokio::test]
async fn workspace_nested_wire_path_uses_slashes() {
    let dir = Directory::new();
    std::fs::create_dir_all(dir.0.join("one").join("two")).unwrap();
    let Frame::WorkspaceFiles { path, .. } = dir.list("one/two").await.unwrap() else {
        panic!("expected listing");
    };
    assert_eq!(path, "one/two");
}
