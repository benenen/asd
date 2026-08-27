//! Test-only git repositories, built by shelling out to `git`.
//!
//! Shelling out rather than writing objects with gix keeps the write-side
//! feature set out of this crate: phase 1 only reads. The temp-directory shape
//! matches `tests/e2e.rs`, which is why this crate does not pull in `tempfile`.

#![cfg(test)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) struct Fixture {
    dir: PathBuf,
    /// Seconds added to each commit's timestamp so ordering is deterministic
    /// rather than dependent on how fast the test machine runs `git`.
    clock: std::cell::Cell<u64>,
}

impl Fixture {
    /// A fresh repository with a deterministic identity and `main` as the
    /// initial branch, so assertions do not depend on the host's git config.
    pub(crate) fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "asd-git-fx-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fx = Self {
            dir,
            clock: std::cell::Cell::new(0),
        };
        fx.git(&["init", "--quiet", "--initial-branch=main"]);
        fx.git(&["config", "user.name", "asd test"]);
        fx.git(&["config", "user.email", "test@example.invalid"]);
        fx.git(&["config", "commit.gpgsign", "false"]);
        fx
    }

    pub(crate) fn path(&self) -> &Path {
        &self.dir
    }

    /// Run a git command, panicking with its stderr when it fails.
    pub(crate) fn git(&self, args: &[&str]) -> String {
        let stamp = 1_700_000_000 + self.clock.get();
        let date = format!("{stamp} +0000");
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.dir)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", &self.dir)
            .output()
            .unwrap_or_else(|e| panic!("running git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// An empty commit with `summary` as its message. Each commit advances the
    /// fixture clock so commit times are strictly increasing.
    pub(crate) fn commit(&self, summary: &str) -> String {
        self.clock.set(self.clock.get() + 60);
        self.git(&["commit", "--quiet", "--allow-empty", "-m", summary]);
        self.git(&["rev-parse", "HEAD"])
    }

    // Unused by phase 1's tests (only `open`/`commit` are needed to test
    // discovery); later tasks build branch/merge topologies to test lane
    // layout with this same fixture.
    #[allow(dead_code)]
    pub(crate) fn branch(&self, name: &str) {
        self.git(&["checkout", "--quiet", "-b", name]);
    }

    #[allow(dead_code)]
    pub(crate) fn checkout(&self, name: &str) {
        self.git(&["checkout", "--quiet", name]);
    }

    /// Merge `name` into the current branch, always creating a merge commit.
    #[allow(dead_code)]
    pub(crate) fn merge(&self, name: &str, summary: &str) -> String {
        self.clock.set(self.clock.get() + 60);
        self.git(&["merge", "--quiet", "--no-ff", "-m", summary, name]);
        self.git(&["rev-parse", "HEAD"])
    }

    #[allow(dead_code)]
    pub(crate) fn tag(&self, name: &str) {
        self.git(&["tag", name]);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}
