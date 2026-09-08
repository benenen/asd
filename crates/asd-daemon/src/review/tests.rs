use super::*;

struct Repo(std::path::PathBuf);
impl Repo {
    fn new() -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("asd-review-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        let repo = Self(path);
        repo.git(&["init", "-b", "main"]);
        repo.git(&["config", "user.name", "Review Test"]);
        repo.git(&["config", "user.email", "review@example.invalid"]);
        repo
    }
    fn git(&self, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.0)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn write(&self, name: &str, content: &str) {
        std::fs::write(self.0.join(name), content).unwrap();
    }
    fn commit(&self) {
        self.git(&["add", "."]);
        self.git(&["commit", "-m", "base"]);
    }
    async fn review(&self) -> Frame {
        collect(SessionIdentity { instance_id: 7 }, None, self.0.clone())
            .await
            .unwrap()
    }
}
impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn review_includes_staged_unstaged_and_untracked() {
    let repo = Repo::new();
    repo.write("tracked.txt", "base\n");
    repo.commit();
    repo.write("tracked.txt", "staged\n");
    repo.git(&["add", "tracked.txt"]);
    repo.write("tracked.txt", "unstaged\n");
    repo.write("untracked.txt", "untracked\n");
    let Frame::SessionReview {
        identity,
        directory,
        branch,
        status,
        diff,
        truncated,
        ..
    } = repo.review().await
    else {
        panic!("review frame")
    };
    assert_eq!(identity.instance_id, 7);
    assert_eq!(
        std::path::Path::new(&directory),
        repo.0.canonicalize().unwrap()
    );
    assert_eq!(branch, "main");
    assert!(status.contains("untracked.txt"));
    assert!(diff.contains("+staged") && diff.contains("+unstaged"));
    assert!(!truncated);
}

#[tokio::test]
async fn review_handles_unborn_and_detached_heads() {
    let repo = Repo::new();
    repo.write("new.txt", "new\n");
    repo.git(&["add", "new.txt"]);
    let Frame::SessionReview { branch, diff, .. } = repo.review().await else {
        panic!("review frame")
    };
    assert!(branch.contains("unborn"));
    assert!(diff.contains("+new"));
    repo.commit();
    repo.git(&["checkout", "--detach"]);
    let Frame::SessionReview { branch, diff, .. } = repo.review().await else {
        panic!("review frame")
    };
    assert!(branch.contains("detached"));
    assert!(diff.is_empty());
}

#[tokio::test]
async fn review_does_not_execute_external_diff_or_textconv() {
    let repo = Repo::new();
    repo.write("tracked.txt", "base\n");
    repo.write(".gitattributes", "*.txt diff=unsafe\n");
    repo.commit();
    repo.git(&["config", "diff.external", "must-not-execute-external-diff"]);
    repo.git(&[
        "config",
        "diff.unsafe.textconv",
        "must-not-execute-textconv",
    ]);
    repo.git(&["config", "core.fsmonitor", "must-not-execute-fsmonitor"]);
    repo.write("tracked.txt", "changed\n");
    let Frame::SessionReview { diff, .. } = repo.review().await else {
        panic!("review frame")
    };
    assert!(diff.contains("+changed"));
}

#[tokio::test]
async fn review_bounds_large_diffs_and_reports_truncation() {
    let repo = Repo::new();
    repo.write("large.txt", "base\n");
    repo.commit();
    repo.write("large.txt", &"new content\n".repeat(100_000));
    let Frame::SessionReview {
        diff, truncated, ..
    } = repo.review().await
    else {
        panic!("review frame")
    };
    assert!(truncated);
    assert!(diff.len() < 2 * DIFF_LIMIT + 256);
}

#[tokio::test]
async fn review_rejects_non_repository() {
    let repo = Repo::new();
    std::fs::remove_dir_all(repo.0.join(".git")).unwrap();
    assert!(
        collect(SessionIdentity { instance_id: 7 }, None, repo.0.clone())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn review_resolves_linked_worktree_and_subdirectories() {
    let repo = Repo::new();
    repo.write("base.txt", "base\n");
    repo.commit();
    let linked = repo.0.join("linked");
    repo.git(&[
        "worktree",
        "add",
        "-b",
        "task-branch",
        linked.to_str().unwrap(),
    ]);
    let subdir = linked.join("nested");
    std::fs::create_dir_all(&subdir).unwrap();
    let Frame::SessionReview {
        directory, branch, ..
    } = collect(SessionIdentity { instance_id: 7 }, None, subdir)
        .await
        .unwrap()
    else {
        panic!("review frame")
    };
    assert_eq!(
        std::path::Path::new(&directory),
        linked.canonicalize().unwrap()
    );
    assert_eq!(branch, "task-branch");
}

#[tokio::test]
async fn review_neutralizes_terminal_control_sequences_in_file_content() {
    let repo = Repo::new();
    repo.write("tracked.txt", "base\n");
    repo.commit();
    repo.write("tracked.txt", "unsafe \u{1b}]52;c;payload\u{7}\n");
    let Frame::SessionReview { diff, .. } = repo.review().await else {
        panic!("review frame")
    };
    assert!(!diff.contains('\u{1b}'));
    assert!(!diff.contains('\u{7}'));
    assert!(diff.contains("payload"));
}

#[tokio::test]
async fn review_does_not_run_clean_or_process_filters() {
    let repo = Repo::new();
    repo.write("tracked.txt", "base\n");
    repo.commit();
    repo.write(".gitattributes", "tracked.txt filter=sideeffect\n");
    repo.git(&[
        "config",
        "filter.sideeffect.clean",
        "touch review-mutated; cat",
    ]);
    repo.git(&["config", "filter.sideeffect.required", "true"]);
    repo.write("tracked.txt", "changed\n");
    let frame = repo.review().await;
    assert!(
        !repo.0.join("review-mutated").exists(),
        "review executed a clean filter"
    );
    let Frame::SessionReview { diff, .. } = frame else {
        panic!("review frame")
    };
    assert!(diff.contains("+changed"));
    repo.git(&[
        "config",
        "filter.sideeffect.process",
        "touch review-mutated; cat",
    ]);
    let frame = repo.review().await;
    assert!(
        !repo.0.join("review-mutated").exists(),
        "review executed a process filter"
    );
    let Frame::SessionReview { diff, .. } = frame else {
        panic!("review frame")
    };
    assert!(diff.contains("+changed"));
}

#[tokio::test]
async fn review_does_not_execute_nested_submodule_filters() {
    let child = Repo::new();
    child.write("file.txt", "base\n");
    child.write(".gitattributes", "file.txt filter=sideeffect\n");
    child.commit();
    let parent = Repo::new();
    parent.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        child.0.to_str().unwrap(),
        "nested",
    ]);
    parent.commit();
    let marker = parent.0.join("filter-ran");
    parent.git(&[
        "-C",
        "nested",
        "config",
        "filter.sideeffect.clean",
        &format!("touch '{}'; cat", marker.display()),
    ]);
    std::fs::write(parent.0.join("nested/file.txt"), "modified\n").unwrap();
    let Frame::SessionReview { diff, .. } = parent.review().await else {
        panic!("expected review")
    };
    assert!(
        diff.is_empty(),
        "nested working-file changes require their own review: {diff}"
    );
    assert!(
        !marker.exists(),
        "review must not execute a nested worktree filter"
    );
}
