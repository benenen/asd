//! Authoritative hook validation and shell-safe, fixed resume commands.

use asd_proto::{AgentHookAction, AgentKind};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentResumeRecord {
    #[serde(with = "stored_kind")]
    pub kind: AgentKind,
    pub session_ref: String,
    pub reported_at_ms: u64,
}

mod stored_kind {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        kind: &AgentKind,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(kind.as_str())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<AgentKind, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "codex" => Ok(AgentKind::Codex),
            "claude" => Ok(AgentKind::Claude),
            _ => Err(serde::de::Error::custom("unsupported stored agent kind")),
        }
    }
}

pub fn validate_reference(value: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 128
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(
            "invalid agent session reference (want [A-Za-z0-9][A-Za-z0-9_-]{0,127})".into(),
        );
    }
    Ok(())
}

pub fn validate_report(
    kind: AgentKind,
    action: &AgentHookAction,
    reference: &str,
) -> Result<(), String> {
    validate_reference(reference)?;
    let allowed = match action {
        AgentHookAction::Start { source } => {
            matches!(source.as_str(), "startup" | "resume" | "clear" | "compact")
                || (kind == AgentKind::Claude && source == "fork")
        }
        AgentHookAction::End { reason } => {
            reason == "other"
                || (kind == AgentKind::Claude
                    && matches!(
                        reason.as_str(),
                        "clear" | "resume" | "logout" | "prompt_input_exit"
                    ))
        }
    };
    if !allowed {
        return Err("unsupported agent hook lifecycle value".into());
    }
    Ok(())
}

pub struct ResumePlan<'a> {
    program: &'static str,
    argv: [&'a str; 2],
}

pub(crate) fn command_kind(command: &str) -> Option<AgentKind> {
    let mut words = command.split_whitespace();
    let mut executable = words.next()?;
    if matches!(
        executable.rsplit(['/', '\\']).next()?,
        "node" | "nodejs" | "node.exe" | "bun" | "bun.exe"
    ) {
        executable = words.next()?;
        // Evaluation flags and arbitrary interpreter options cannot prove a script.
        if executable.starts_with('-') {
            return None;
        }
        if executable.ends_with("/@anthropic-ai/claude-code/cli.js") {
            return Some(AgentKind::Claude);
        }
    }
    match executable.rsplit(['/', '\\']).next()? {
        "codex" | "codex.exe" => Some(AgentKind::Codex),
        "claude" | "claude.exe" => Some(AgentKind::Claude),
        _ => None,
    }
}

pub(crate) fn resume_owner<'a>(
    states: &'a [crate::store::SessionState],
    record: &AgentResumeRecord,
) -> Option<&'a str> {
    states
        .iter()
        .filter(|state| {
            state
                .agent_resume
                .as_ref()
                .is_some_and(|r| r.kind == record.kind && r.session_ref == record.session_ref)
        })
        .min_by(|a, b| {
            b.agent_resume
                .as_ref()
                .unwrap()
                .reported_at_ms
                .cmp(&a.agent_resume.as_ref().unwrap().reported_at_ms)
                .then_with(|| a.name.cmp(&b.name))
        })
        .map(|state| state.name.as_str())
}

impl<'a> ResumePlan<'a> {
    pub fn for_record(record: &'a AgentResumeRecord) -> Result<Self, String> {
        validate_reference(&record.session_ref)?;
        let (program, flag) = match record.kind {
            AgentKind::Codex => ("codex", "resume"),
            AgentKind::Claude => ("claude", "--resume"),
        };
        Ok(Self {
            program,
            argv: [flag, &record.session_ref],
        })
    }
    pub fn program(&self) -> &str {
        self.program
    }
    pub fn argv(&self) -> [&str; 2] {
        self.argv
    }
    pub fn display(&self) -> String {
        format!("{} {} {}", self.program, self.argv[0], self.argv[1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asd_proto::{AgentHookAction, AgentKind};

    #[test]
    fn recognized_interpreter_launches_do_not_accept_eval_or_arbitrary_scripts() {
        assert_eq!(
            command_kind("node /usr/local/bin/codex"),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            command_kind("node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"),
            Some(AgentKind::Claude)
        );
        for command in [
            "node -e codex",
            "echo codex",
            "node /tmp/cli.js",
            "codex; evil",
            "sh -c codex",
            "unknown",
        ] {
            assert_eq!(command_kind(command), None);
        }
    }

    #[test]
    fn fixed_resume_commands_and_reference_grammar() {
        for (kind, expected) in [
            (AgentKind::Codex, "codex resume thr_123"),
            (AgentKind::Claude, "claude --resume thr_123"),
        ] {
            let record = AgentResumeRecord {
                kind,
                session_ref: "thr_123".into(),
                reported_at_ms: 1,
            };
            assert_eq!(ResumePlan::for_record(&record).unwrap().display(), expected);
        }
        for value in ["", "-x", "_x", "a;b", "a b", "a\n", "é", &"a".repeat(129)] {
            assert!(validate_reference(value).is_err(), "{value:?}");
        }
        assert!(validate_reference(&"a".repeat(128)).is_ok());
        assert!(validate_reference("0-a_Z").is_ok());
    }

    #[test]
    fn duplicate_claims_choose_newest_then_ascending_name() {
        let make = |name: &str, time| crate::store::SessionState {
            name: name.into(),
            command: Some("original".into()),
            cwd: None,
            agent_resume: Some(AgentResumeRecord {
                kind: AgentKind::Codex,
                session_ref: "same".into(),
                reported_at_ms: time,
            }),
        };
        let states = vec![make("z", 20), make("old", 10), make("a", 20)];
        assert_eq!(
            resume_owner(&states, states[0].agent_resume.as_ref().unwrap()),
            Some("a")
        );
    }

    #[test]
    fn exact_lifecycle_allowlists_at_daemon_boundary() {
        for kind in [AgentKind::Codex, AgentKind::Claude] {
            for source in [
                "startup", "resume", "clear", "compact", "fork", "other", "unknown", "",
            ] {
                assert_eq!(
                    validate_report(
                        kind,
                        &AgentHookAction::Start {
                            source: source.into()
                        },
                        "id"
                    )
                    .is_ok(),
                    matches!(source, "startup" | "resume" | "clear" | "compact")
                        || (kind == AgentKind::Claude && source == "fork")
                );
            }
            for reason in [
                "clear",
                "resume",
                "logout",
                "prompt_input_exit",
                "other",
                "startup",
                "unknown",
                "",
            ] {
                assert_eq!(
                    validate_report(
                        kind,
                        &AgentHookAction::End {
                            reason: reason.into()
                        },
                        "id"
                    )
                    .is_ok(),
                    reason == "other"
                        || (kind == AgentKind::Claude
                            && matches!(
                                reason,
                                "clear" | "resume" | "logout" | "prompt_input_exit"
                            ))
                );
            }
        }
    }
}
