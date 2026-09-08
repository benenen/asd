use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;

use asd_proto::{Frame, SessionIdentity, WorkspaceEntryKind};

#[tokio::test]
async fn workspace_lists_symlinks_but_rejects_navigation_and_invalid_utf8() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("asd-files-unix-{unique}"));
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    symlink("nested", root.join("internal")).unwrap();
    symlink(std::env::temp_dir(), root.join("external")).unwrap();
    let identity = SessionIdentity { instance_id: 8 };
    let Frame::WorkspaceFiles { entries, .. } =
        crate::workspace::collect(identity, root.clone(), String::new())
            .await
            .unwrap()
    else {
        panic!("expected listing");
    };
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.kind == WorkspaceEntryKind::Symlink)
            .count(),
        2
    );
    for path in ["internal", "external", "external/child"] {
        let error = crate::workspace::collect(identity, root.clone(), path.into())
            .await
            .unwrap_err();
        assert!(error.contains("symbolic links"));
    }
    std::fs::write(root.join(std::ffi::OsString::from_vec(vec![0xff])), "").unwrap();
    let error = crate::workspace::collect(identity, root.clone(), String::new())
        .await
        .unwrap_err();
    assert!(error.contains("not valid UTF-8"));
    std::fs::remove_dir_all(root).unwrap();
}
