//! Task associations and review use real daemon-owned Git worktrees.
use super::*;

fn git_at(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
}

#[tokio::test]
async fn task_review_tracks_linked_worktree_and_survives_rename_restart_clear() {
    let daemon = Daemon::start("task-review");
    let repo = daemon.dir.join("repo");
    let worktree = daemon.dir.join("worktree");
    std::fs::create_dir(&repo).unwrap();
    git_at(&repo, &["init", "-q", "-b", "main"]);
    git_at(&repo, &["config", "user.name", "Test"]);
    git_at(&repo, &["config", "user.email", "test@example.invalid"]);
    std::fs::write(repo.join("tracked.txt"), "base\n").unwrap();
    git_at(&repo, &["add", "."]);
    git_at(
        &repo,
        &["-c", "commit.gpgsign=false", "commit", "-qm", "base"],
    );
    git_at(
        &repo,
        &[
            "worktree",
            "add",
            "-qb",
            "task-branch",
            worktree.to_str().unwrap(),
        ],
    );
    std::fs::write(worktree.join("tracked.txt"), "staged\n").unwrap();
    git_at(&worktree, &["add", "tracked.txt"]);
    std::fs::write(worktree.join("tracked.txt"), "staged\nunstaged\n").unwrap();
    std::fs::write(worktree.join("new.txt"), "untracked\n").unwrap();
    let out = daemon
        .cli()
        .args(["new", "task", "--cwd", repo.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let out = daemon
        .cli()
        .args([
            "task",
            "task",
            "--description",
            "修复登录问题",
            "--directory",
            worktree.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "task association must be accepted: {out:?}"
    );
    let out = daemon
        .cli()
        .args(["rename", "task", "renamed"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let sentinel = daemon.dir.join("external-diff-ran");
    let helper = daemon.dir.join("diff-helper.sh");
    std::fs::write(
        &helper,
        format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
    )
    .unwrap();
    git_at(
        &worktree,
        &[
            "config",
            "diff.external",
            &format!("sh {}", helper.display()),
        ],
    );
    let invalid = daemon
        .cli()
        .args([
            "task",
            "renamed",
            "--description",
            "replacement",
            "--directory",
            "/__asd_missing_task_worktree__",
        ])
        .output()
        .unwrap();
    assert!(
        !invalid.status.success(),
        "invalid association must be rejected"
    );
    let review = daemon
        .cli()
        .args(["review", "renamed", "--json"])
        .output()
        .unwrap();
    assert!(review.status.success(), "{review:?}");
    assert!(
        !sentinel.exists(),
        "opening review must not run external diff helpers"
    );
    let value: serde_json::Value = serde_json::from_slice(&review.stdout).unwrap();
    assert_eq!(value["task"]["description"], "修复登录问题");
    assert_eq!(value["branch"], "task-branch");
    assert_eq!(
        value["directory"],
        worktree.canonicalize().unwrap().to_str().unwrap()
    );
    assert!(value["diff"].as_str().unwrap().contains("+unstaged"));
    assert!(value["diff"].as_str().unwrap().contains("+staged"));
    assert!(value["status"].as_str().unwrap().contains("new.txt"));
    let before = daemon.cli().args(["list", "--json"]).output().unwrap();
    let rows: serde_json::Value = serde_json::from_slice(&before.stdout).unwrap();
    assert_eq!(rows[0]["attached_clients"], 0);
    assert_eq!(rows[0]["task"]["description"], "修复登录问题");
    daemon.stop_and_wait();
    let mut successor = daemon.respawn_successor();
    let out = daemon
        .cli()
        .args(["task", "renamed", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(task["description"], "修复登录问题");
    let clear = daemon
        .cli()
        .args(["task", "renamed", "--clear"])
        .output()
        .unwrap();
    assert!(clear.status.success(), "{clear:?}");
    let out = daemon
        .cli()
        .args(["task", "renamed", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),
        serde_json::Value::Null
    );
    unsafe {
        libc::kill(successor.id() as i32, libc::SIGTERM);
    }
    successor.wait().unwrap();
}

#[tokio::test]
async fn task_review_reports_non_repository_and_missing_session() {
    let daemon = Daemon::start("task-errors");
    assert!(
        daemon
            .cli()
            .args(["new", "plain", "--cwd", daemon.dir.to_str().unwrap()])
            .output()
            .unwrap()
            .status
            .success()
    );
    let out = daemon.cli().args(["review", "plain"]).output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("git")
            || String::from_utf8_lossy(&out.stderr).contains("repository")
    );
    let out = daemon
        .cli()
        .args(["task", "missing", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
}
