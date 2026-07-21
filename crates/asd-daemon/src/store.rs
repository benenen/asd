//! Persistent session list. The daemon keeps `{name, cwd}` for every live
//! session in a data-dir file (`paths::session_list_path`), rewritten on every
//! create/rename/kill and restored on every startup — each session comes back as
//! a fresh shell `cd`'d to its saved directory. Only the cwd is restored, not the
//! live process or the screen.
//!
//! Spec: docs/superpowers/specs/2026-07-21-persistent-session-list-design.md

use std::path::{Path, PathBuf};

/// One session's entry in the persisted list: its name and cwd (if readable).
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

/// Atomically write the session list: write a sibling temp file, then `rename`
/// it over `path` (atomic on the same filesystem), so a crash mid-write cannot
/// leave a torn file. Best effort — a failure only logs a warning.
pub fn write_atomic(path: &Path, states: &[SessionState]) {
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, serialize(states)) {
        tracing::warn!(path = %tmp.display(), error = %e, "failed to write session list");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        tracing::warn!(path = %path.display(), error = %e, "failed to install session list");
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Read and parse the session list, WITHOUT deleting it — the file is the live
/// source of truth, not consume-once. An absent/unreadable file yields an empty
/// list.
pub fn read(path: &Path) -> Vec<SessionState> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(_) => Vec::new(),
    }
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

    #[test]
    fn write_atomic_then_read_round_trips() {
        let dir = std::env::temp_dir().join(format!("asd-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.tsv");
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
        write_atomic(&path, &states);
        assert_eq!(read(&path), states);
        // An absent file reads as an empty list.
        std::fs::remove_file(&path).unwrap();
        assert!(read(&path).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
