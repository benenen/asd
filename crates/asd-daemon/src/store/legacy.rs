use std::path::PathBuf;

use super::SessionState;

#[cfg(test)]
pub(crate) fn serialize(states: &[SessionState]) -> String {
    let mut out = String::new();
    for state in states {
        let cwd = state
            .cwd
            .as_deref()
            .map(|path| path.to_string_lossy())
            .unwrap_or_default();
        out.push_str(&state.name);
        out.push('\t');
        out.push_str(&cwd);
        out.push('\t');
        out.push_str(&escape(state.command.as_deref().unwrap_or_default()));
        out.push('\n');
    }
    out
}

pub(super) fn parse_with_diagnostics(text: &str) -> (Vec<SessionState>, Vec<String>) {
    let mut states = Vec::new();
    let mut diagnostics = Vec::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let mut fields = line.splitn(3, '\t');
        let name = fields.next().unwrap_or_default();
        let cwd = fields.next().unwrap_or_default();
        let command = fields.next().unwrap_or_default();
        if name.is_empty() {
            diagnostics.push(format!("legacy session line {} has no name", index + 1));
            continue;
        }
        states.push(SessionState {
            task: None,
            agent_resume: None,
            name: name.to_string(),
            cwd: (!cwd.is_empty()).then(|| PathBuf::from(cwd)),
            command: (!command.is_empty()).then(|| unescape(command)),
        });
    }
    (states, diagnostics)
}

#[cfg(test)]
pub(crate) fn parse(text: &str) -> Vec<SessionState> {
    parse_with_diagnostics(text).0
}

#[cfg(test)]
fn escape(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    for character in command.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(character),
        }
    }
    out
}

fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut characters = field.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
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
