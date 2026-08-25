//! Persistent session list. The daemon keeps `{name, cwd, command}` for every
//! live session in a data-dir file (`paths::session_list_path`), rewritten on
//! every create/rename/kill and restored on every startup — each session comes
//! back as a fresh shell `cd`'d to its saved directory, with the command it was
//! created with waiting at that shell's prompt. Neither the live process nor the
//! screen is restored, and the command is not re-run on its own.

use std::path::{Path, PathBuf};

/// One session's entry in the persisted list: its name, cwd (if readable), and
/// the command it was created with (if it was given one rather than a shell).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    pub name: String,
    pub cwd: Option<PathBuf>,
    /// What `--cmd` asked for. `None` for a plain shell session, which is what
    /// most sessions are and what every entry written before this field existed
    /// parses as.
    pub command: Option<String>,
}

/// The cwd of a live process, for the persisted session list. `None` when it
/// cannot be determined — the session then recreates in the daemon's default
/// directory rather than failing. Platform detail in `platform::read_cwd`.
///
/// Re-exported from the crate root because `asd card` resolves a session's
/// project directory the same way: one platform implementation, so the CLI and
/// the persisted list cannot disagree about where a session is.
pub fn read_cwd(pid: u32) -> Option<PathBuf> {
    crate::platform::read_cwd(pid)
}

/// One `name\tcwd\tcommand` line per session; either trailing field may be
/// empty. Names are `[A-Za-z0-9_-]` and paths don't contain tabs/newlines in
/// practice, so those two fields are written raw and a tab-separated line stays
/// unambiguous. A command is arbitrary user text that may contain both, so it —
/// and only it — is escaped; the cwd deliberately is not, because decoding a
/// field written raw by an older daemon would corrupt a Windows path like
/// `C:\tools`.
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
        out.push('\t');
        out.push_str(&escape(s.command.as_deref().unwrap_or_default()));
        out.push('\n');
    }
    out
}

/// Escape the command field so a tab or newline in it cannot invent a field or
/// a line. Only these four sequences are produced, and [`unescape`] leaves any
/// other backslash alone.
fn escape(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    for c in cmd.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

/// Inverse of [`escape`]. An unknown escape (`\p`) keeps both characters, so a
/// hand-edited line loses nothing.
fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
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
            let mut fields = line.splitn(3, '\t');
            let name = fields.next().unwrap_or_default();
            let cwd = fields.next().unwrap_or_default();
            let command = fields.next().unwrap_or_default();
            if name.is_empty() {
                return None;
            }
            Some(SessionState {
                name: name.to_string(),
                cwd: (!cwd.is_empty()).then(|| PathBuf::from(cwd)),
                command: (!command.is_empty()).then(|| unescape(command)),
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
    fn round_trips_names_cwds_and_commands() {
        let states = vec![
            SessionState {
                name: "web".into(),
                cwd: Some(PathBuf::from("/home/me/proj")),
                command: Some("npm run dev".into()),
            },
            SessionState {
                name: "s0".into(),
                cwd: None,
                command: None,
            },
        ];
        assert_eq!(parse(&serialize(&states)), states);
    }

    /// A command is arbitrary text. A tab in it must not invent a field and a
    /// newline must not invent an entry.
    #[test]
    fn a_command_survives_tabs_newlines_and_backslashes() {
        let states = vec![SessionState {
            name: "odd".into(),
            cwd: Some(PathBuf::from("/tmp")),
            command: Some("printf 'a\tb\n' && grep -E '\\d+' C:\\tools".into()),
        }];
        let text = serialize(&states);
        assert_eq!(
            text.lines().count(),
            1,
            "escaped command stayed on one line"
        );
        assert_eq!(parse(&text), states);
    }

    /// Lines written before the command field existed still parse, and their
    /// cwd is read exactly as written — a Windows path is not an escape
    /// sequence.
    #[test]
    fn two_field_lines_parse_as_commandless() {
        assert_eq!(
            parse("web\t/home/me/proj\nwin\tC:\\tools\n"),
            vec![
                SessionState {
                    name: "web".into(),
                    cwd: Some(PathBuf::from("/home/me/proj")),
                    command: None,
                },
                SessionState {
                    name: "win".into(),
                    cwd: Some(PathBuf::from("C:\\tools")),
                    command: None,
                },
            ]
        );
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
                    command: None,
                },
                SessionState {
                    name: "logs".into(),
                    cwd: None,
                    command: None,
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
                command: Some("claude".into()),
            },
            SessionState {
                name: "s0".into(),
                cwd: None,
                command: None,
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
