//! Test-only git repositories, built by shelling out to `git`.
//!
//! Shelling out rather than writing objects with gix keeps the write-side
//! feature set out of this crate: phase 1 only reads. The temp-directory shape
//! matches `tests/e2e/common.rs`, which is why this crate does not pull in
//! `tempfile`.

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
        // Pin the diff algorithm in the repository's own config, which is the one
        // place both halves of a diff oracle read. `git_raw` isolates the
        // shelled-out git from the host (`HOME`, `GIT_CONFIG_NOSYSTEM`), but
        // `Repo::open` goes through `gix::discover`, which still picks up the
        // developer's real `~/.gitconfig`. Without this line a machine with a
        // global `diff.algorithm` fails the comparison for a reason that has
        // nothing to do with the code under test.
        fx.git(&["config", "diff.algorithm", "myers"]);
        fx
    }

    pub(crate) fn path(&self) -> &Path {
        &self.dir
    }

    /// Run a git command, panicking with its stderr when it fails. The output
    /// is trimmed, which is what a caller reading back an object id wants.
    pub(crate) fn git(&self, args: &[&str]) -> String {
        self.git_raw(args).trim().to_string()
    }

    /// [`Fixture::git`] without the trim, for callers reading diff *text*.
    ///
    /// A trailing blank context line is a single space, and a leading one is a
    /// space too: trimming deletes both, so an oracle built on the trimmed
    /// form silently drops the very rows it exists to compare.
    pub(crate) fn git_raw(&self, args: &[&str]) -> String {
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
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// An empty commit with `summary` as its message. Each commit advances the
    /// fixture clock so commit times are strictly increasing.
    pub(crate) fn commit(&self, summary: &str) -> String {
        self.clock.set(self.clock.get() + 60);
        self.git(&["commit", "--quiet", "--allow-empty", "-m", summary]);
        self.git(&["rev-parse", "HEAD"])
    }

    pub(crate) fn branch(&self, name: &str) {
        self.git(&["checkout", "--quiet", "-b", name]);
    }

    pub(crate) fn checkout(&self, name: &str) {
        self.git(&["checkout", "--quiet", name]);
    }

    /// Merge `name` into the current branch, always creating a merge commit.
    pub(crate) fn merge(&self, name: &str, summary: &str) -> String {
        self.merge_many(&[name], summary)
    }

    /// Merge every branch in `names` into the current branch in one commit,
    /// always creating a merge commit. Two or more names make an octopus.
    ///
    /// This exists so an octopus does not have to be spelled as a raw `git`
    /// call: `git` alone does not advance the fixture clock, which would leave
    /// the merge sharing a commit time with the branch tip it merges.
    pub(crate) fn merge_many(&self, names: &[&str], summary: &str) -> String {
        self.clock.set(self.clock.get() + 60);
        let mut args = vec!["merge", "--quiet", "--no-ff", "-m", summary];
        args.extend_from_slice(names);
        self.git(&args);
        self.git(&["rev-parse", "HEAD"])
    }

    pub(crate) fn tag(&self, name: &str) {
        self.git(&["tag", name]);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}
