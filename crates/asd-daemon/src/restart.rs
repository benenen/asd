//! `asd restart` workspace preservation. On SIGUSR1 the daemon records each
//! session's name + working directory to a per-socket state file, then shuts
//! down normally; the successor daemon reads that file and recreates the
//! sessions — a fresh shell `cd`'d to the saved directory. Only the cwd is
//! restored, not the live process or the screen.
//!
//! Spec: docs/superpowers/specs/2026-07-21-restart-preserve-workspace-design.md

use std::path::{Path, PathBuf};

/// One session's restorable workspace: its name and cwd (if readable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    pub name: String,
    pub cwd: Option<PathBuf>,
}

/// The cwd of a live process, read from `/proc/<pid>/cwd`. Returns `None` on any
/// error or platform without `/proc` (macOS) — the session then recreates in the
/// daemon's default directory rather than failing.
pub fn read_cwd(pid: u32) -> Option<PathBuf> {
    if pid == 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// One `name\tcwd` line per session (cwd left empty when unknown). Names are
/// `[A-Za-z0-9_-]` and paths don't contain tabs/newlines in practice, so a
/// tab-separated line is unambiguous.
pub fn serialize(states: &[SessionState]) -> String {
    let mut out = String::new();
    for s in states {
        let cwd = s
            .cwd
            .as_deref()
            .map(|p| p.to_string_lossy())
            .unwrap_or_default();
        out.push_str(&s.name);
        out.push('\t');
        out.push_str(&cwd);
        out.push('\n');
    }
    out
}

/// Parse the state file; blank/malformed (nameless) lines are skipped.
pub fn parse(text: &str) -> Vec<SessionState> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                return None;
            }
            let (name, cwd) = line.split_once('\t').unwrap_or((line, ""));
            if name.is_empty() {
                return None;
            }
            Some(SessionState {
                name: name.to_string(),
                cwd: (!cwd.is_empty()).then(|| PathBuf::from(cwd)),
            })
        })
        .collect()
}

/// Write the state file (best effort; a failure just means workspaces aren't
/// restored on the next start).
pub fn write(path: &Path, states: &[SessionState]) {
    if let Err(e) = std::fs::write(path, serialize(states)) {
        tracing::warn!(path = %path.display(), error = %e, "failed to write restart state");
    }
}

/// Read and consume (delete) the state file, returning the sessions to
/// recreate. An absent file yields an empty list. Consume-once so a later normal
/// start doesn't resurrect stale sessions.
pub fn take(path: &Path) -> Vec<SessionState> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let _ = std::fs::remove_file(path);
    parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_names_and_cwds() {
        let states = vec![
            SessionState {
                name: "web".into(),
                cwd: Some(PathBuf::from("/home/me/proj")),
            },
            SessionState {
                name: "s0".into(),
                cwd: None,
            },
        ];
        assert_eq!(parse(&serialize(&states)), states);
    }

    #[test]
    fn skips_blank_and_nameless_lines() {
        let got = parse("web\t/tmp\n\n\t/orphaned\nlogs\t\n");
        assert_eq!(
            got,
            vec![
                SessionState {
                    name: "web".into(),
                    cwd: Some(PathBuf::from("/tmp")),
                },
                SessionState {
                    name: "logs".into(),
                    cwd: None,
                },
            ]
        );
    }

    #[test]
    fn read_cwd_zero_pid_is_none() {
        assert_eq!(read_cwd(0), None);
    }
}
