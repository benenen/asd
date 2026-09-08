use super::*;
use crate::git::fixture::Fixture;

#[test]
fn staged_and_unstaged_changes_are_distinct_even_when_they_cancel() {
    let fx = Fixture::new("working-cancel");
    std::fs::write(fx.path().join("a.txt"), "base\n").unwrap();
    fx.git(&["add", "."]);
    fx.commit("base");
    std::fs::write(fx.path().join("a.txt"), "staged\n").unwrap();
    fx.git(&["add", "."]);
    std::fs::write(fx.path().join("a.txt"), "base\n").unwrap();
    let repo = Repo::open(fx.path()).unwrap();
    let diff = repo.working_diff().unwrap();
    assert_eq!(diff.files.len(), 2);
    assert_eq!(diff.files[0].stage, Some(WorktreeStage::Staged));
    assert_eq!(diff.files[1].stage, Some(WorktreeStage::Unstaged));
    for stage in [WorktreeStage::Staged, WorktreeStage::Unstaged] {
        let file = repo.working_file_diff("a.txt", stage, 3).unwrap();
        assert!(
            file.lines
                .iter()
                .any(|line| matches!(line, DiffLine::Added { .. }))
        );
        assert!(
            file.lines
                .iter()
                .any(|line| matches!(line, DiffLine::Removed { .. }))
        );
    }
}

#[test]
fn unborn_staged_and_nested_untracked_files_are_reviewable() {
    let fx = Fixture::new("working-unborn");
    std::fs::write(fx.path().join("staged.txt"), "staged\n").unwrap();
    fx.git(&["add", "."]);
    std::fs::create_dir(fx.path().join("nested")).unwrap();
    std::fs::write(fx.path().join("nested/new.txt"), "new\n").unwrap();
    let repo = Repo::open(fx.path()).unwrap();
    let diff = repo.working_diff().unwrap();
    assert_eq!(diff.files.len(), 2);
    assert!(
        diff.files.iter().all(|file| file.unreadable.is_none()),
        "{diff:?}"
    );
    assert!(
        repo.working_file_diff("staged.txt", WorktreeStage::Staged, 3)
            .unwrap()
            .lines
            .iter()
            .any(|line| matches!(line, DiffLine::Added { text, .. } if text == "staged"))
    );
}

#[test]
fn inspection_does_not_execute_filters_or_write_index() {
    let fx = Fixture::new("working-no-filter");
    std::fs::write(fx.path().join("a.txt"), "base\n").unwrap();
    fx.git(&["add", "."]);
    fx.commit("base");
    std::fs::write(fx.path().join(".gitattributes"), "a.txt filter=unsafe\n").unwrap();
    fx.git(&["config", "filter.unsafe.clean", "touch invoked; cat"]);
    fx.git(&["config", "filter.unsafe.process", "touch invoked; cat"]);
    fx.git(&["config", "filter.unsafe.required", "true"]);
    std::fs::write(fx.path().join("a.txt"), "changed\n").unwrap();
    let index_before = std::fs::read(fx.path().join(".git/index")).unwrap();
    let config_before = std::fs::read(fx.path().join(".git/config")).unwrap();
    let repo = Repo::open(fx.path()).unwrap();
    let diff = repo.working_diff().unwrap();
    assert!(diff.files.iter().any(|file| file.path == "a.txt"));
    assert!(!fx.path().join("invoked").exists());
    assert_eq!(
        std::fs::read(fx.path().join(".git/index")).unwrap(),
        index_before
    );
    assert_eq!(
        std::fs::read(fx.path().join(".git/config")).unwrap(),
        config_before
    );
}

#[test]
fn staged_rename_lists_both_sides_and_deleted_file_has_removals() {
    let fx = Fixture::new("working-rename");
    std::fs::write(fx.path().join("old.txt"), "base\n").unwrap();
    fx.git(&["add", "."]);
    fx.commit("base");
    fx.git(&["mv", "old.txt", "new.txt"]);
    let repo = Repo::open(fx.path()).unwrap();
    let diff = repo.working_diff().unwrap();
    assert_eq!(diff.files.len(), 2);
    assert!(
        diff.files
            .iter()
            .any(|file| file.path == "old.txt" && file.change == FileChange::Deleted)
    );
    assert!(
        diff.files
            .iter()
            .any(|file| file.path == "new.txt" && file.change == FileChange::Added)
    );
    let old = repo
        .working_file_diff("old.txt", WorktreeStage::Staged, 3)
        .unwrap();
    assert!(
        old.lines
            .iter()
            .any(|line| matches!(line, DiffLine::Removed { .. }))
    );
}

#[test]
fn non_utf8_filename_is_not_an_actionable_lossy_path() {
    let stat = file_stat(b"bad\xff.txt".as_slice().into(), WorktreeStage::Untracked);
    assert!(stat.stage.is_none(), "a lossy filename must not be opened");
    assert!(stat.unreadable.is_some());
}

#[test]
fn oversized_blob_is_rejected_from_header_before_body_decoding() {
    let fx = Fixture::new("working-large-header");
    std::fs::write(fx.path().join("large.txt"), vec![b'x'; MAX_FILE_BYTES + 1]).unwrap();
    let id = fx.git(&["hash-object", "-w", "large.txt"]);
    let object = fx.path().join(".git/objects").join(&id[..2]).join(&id[2..]);
    let bytes = std::fs::read(&object).unwrap();
    // Preserve the header but corrupt the trailer: decoding the body must fail.
    std::fs::write(object, &bytes[..bytes.len() - 8]).unwrap();
    let repo = Repo::open(fx.path()).unwrap();
    let error = repo.read_blob(id.parse().unwrap()).unwrap_err();
    assert!(
        error.to_string().contains("8 MiB"),
        "body was decoded before the limit: {error}"
    );
}
