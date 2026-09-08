//! Agent detection diagnostics and typed, authoritative lifecycle hooks.

use anyhow::{Context, bail};
use asd_proto::{AgentHookAction, AgentKind, ClientKind, Frame, SessionIdentity};
use clap::{Subcommand, ValueEnum};
use std::io::Read;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Phase {
    Start,
    End,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Explain the live session screen using the daemon's current rules.
    Explain {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Reload agent manifests and wait up to five seconds for existing sessions.
    Reload {
        #[arg(long)]
        json: bool,
    },
    /// Read an authoritative agent lifecycle payload from stdin.
    Hook { kind: AgentKind, phase: Phase },
    /// Remove resume metadata for the exact session hosting this command.
    Clear,
}

fn parse_hook(
    kind: AgentKind,
    phase: Phase,
    input: &str,
) -> anyhow::Result<(AgentHookAction, String)> {
    let value: serde_json::Value = serde_json::from_str(input).context("invalid hook JSON")?;
    let object = value
        .as_object()
        .context("hook payload must be one JSON object")?;
    let field = |name: &str| {
        object
            .get(name)
            .and_then(serde_json::Value::as_str)
            .with_context(|| format!("hook requires string {name}"))
    };
    let reference = field("session_id")?.to_owned();
    let (event, action) = match phase {
        Phase::Start => (
            "SessionStart",
            AgentHookAction::Start {
                source: field("source")?.into(),
            },
        ),
        Phase::End => (
            "SessionEnd",
            AgentHookAction::End {
                reason: field("reason")?.into(),
            },
        ),
    };
    if field("hook_event_name")? != event {
        bail!("hook event does not match requested phase");
    }
    asd_daemon::agent_resume::validate_report(kind, &action, &reference)
        .map_err(anyhow::Error::msg)?;
    Ok((action, reference))
}

pub async fn run(socket: &std::path::Path, command: Command) -> anyhow::Result<()> {
    let command = match command {
        Command::Explain { name, json } => {
            return diagnostics(socket, Frame::AgentExplain { name }, json).await;
        }
        Command::Reload { json } => {
            return diagnostics(socket, Frame::ReloadAgentManifests, json).await;
        }
        hook => hook,
    };
    let identity: SessionIdentity = std::env::var("ASD_SESSION_ID")
        .context("ASD_SESSION_ID is required; run inside an asd session")?
        .parse()
        .map_err(anyhow::Error::msg)?;
    let (frame, expected) = match command {
        Command::Hook { kind, phase } => {
            let mut input = String::new();
            std::io::stdin()
                .take(65537)
                .read_to_string(&mut input)
                .context("reading hook stdin")?;
            if input.len() > 65536 {
                bail!("hook payload exceeds 64 KiB");
            }
            let (action, session_ref) = parse_hook(kind, phase, &input)?;
            (
                Frame::ReportAgentSession {
                    identity,
                    kind,
                    action,
                    session_ref,
                },
                Frame::AgentSessionReported,
            )
        }
        Command::Clear => (
            Frame::ClearAgentSession { identity },
            Frame::AgentSessionCleared,
        ),
        Command::Explain { .. } | Command::Reload { .. } => {
            unreachable!("diagnostics handled above")
        }
    };
    let mut client = crate::client::connect(socket, ClientKind::Cli).await?;
    client.writer.write_frame(&frame).await?;
    match client.reader.read_frame().await? {
        Some(reply) if reply == expected => Ok(()),
        Some(Frame::Error { code, msg }) => bail!("agent report rejected ({code}): {msg}"),
        _ => bail!("daemon closed or returned an unexpected agent acknowledgement"),
    }
}

async fn diagnostics(socket: &std::path::Path, request: Frame, json: bool) -> anyhow::Result<()> {
    let mut client = crate::client::connect(socket, ClientKind::Cli).await?;
    client.writer.write_frame(&request).await?;
    let reply = client
        .reader
        .read_frame()
        .await?
        .context("daemon closed before agent diagnostics reply")?;
    match (request, reply) {
        (Frame::AgentExplain { .. }, Frame::AgentExplainReply { report }) => {
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!(
                    "command: {:?}\nstate: {}\ngeneration: {}",
                    report.foreground_command, report.state, report.generation
                );
                println!(
                    "candidates: {}\nselected: {}",
                    report.candidate_manifest_ids.join(", "),
                    report.selected_manifest_id.as_deref().unwrap_or("none")
                );
                for rule in report.rules {
                    println!(
                        "{} / {}: {} priority={} region={} matched={}",
                        rule.manifest_id,
                        rule.rule_id,
                        rule.state,
                        rule.priority,
                        rule.region,
                        rule.matched
                    );
                    for evidence in rule.evidence {
                        println!("  evidence: {evidence:?}");
                    }
                    if let Some(reason) = rule.reason {
                        println!("  reason: {reason}");
                    }
                }
            }
            Ok(())
        }
        (
            Frame::ReloadAgentManifests,
            Frame::AgentManifestsReloaded {
                generation,
                diagnostics,
                pending_identities,
            },
        ) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "generation": generation, "diagnostics": diagnostics, "pending_identities": pending_identities })
                );
            } else {
                println!(
                    "detector generation {generation} active; {} sessions pending",
                    pending_identities.len()
                );
                for diagnostic in &diagnostics {
                    println!(
                        "{}: {} (retained previous: {})",
                        diagnostic.path, diagnostic.message, diagnostic.retained_previous
                    );
                }
                for identity in &pending_identities {
                    println!("pending: {identity}");
                }
            }
            if !pending_identities.is_empty() {
                bail!(
                    "detector generation {generation} is active; some sessions have not acknowledged it"
                );
            }
            Ok(())
        }
        (_, Frame::Error { code, msg }) => {
            Err(crate::exit::daemon("agent diagnostics", code, &msg))
        }
        _ => bail!("daemon returned an unexpected agent diagnostics reply"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_enforces_each_vendor_lifecycle_allowlist() {
        for kind in [AgentKind::Codex, AgentKind::Claude] {
            for value in [
                "startup",
                "resume",
                "clear",
                "compact",
                "fork",
                "other",
                "logout",
                "prompt_input_exit",
                "unknown",
                "",
            ] {
                let start = format!(
                    r#"{{"session_id":"id","hook_event_name":"SessionStart","source":"{value}"}}"#
                );
                let end = format!(
                    r#"{{"session_id":"id","hook_event_name":"SessionEnd","reason":"{value}"}}"#
                );
                assert_eq!(
                    parse_hook(kind, Phase::Start, &start).is_ok(),
                    matches!(value, "startup" | "resume" | "clear" | "compact")
                        || (kind == AgentKind::Claude && value == "fork")
                );
                assert_eq!(
                    parse_hook(kind, Phase::End, &end).is_ok(),
                    value == "other"
                        || (kind == AgentKind::Claude
                            && matches!(
                                value,
                                "clear" | "resume" | "logout" | "prompt_input_exit"
                            ))
                );
            }
        }
    }

    #[test]
    fn official_hook_payloads_and_extra_fields() {
        let (action, reference) = parse_hook(AgentKind::Codex, Phase::Start, r#"{"session_id":"thr_123","hook_event_name":"SessionStart","source":"startup","cwd":"/work"}"#).unwrap();
        assert_eq!(reference, "thr_123");
        assert_eq!(
            action,
            AgentHookAction::Start {
                source: "startup".into()
            }
        );
        assert!(parse_hook(AgentKind::Claude, Phase::End, r#"{"session_id":"550e8400-e29b-41d4-a716-446655440000","hook_event_name":"SessionEnd","reason":"other","cwd":"/work"}"#).is_ok());
    }

    #[test]
    fn rejects_missing_wrong_typed_and_mismatched_fields() {
        for json in [
            "[]",
            "null",
            "{}",
            r#"{"session_id":7,"hook_event_name":"SessionStart","source":"startup"}"#,
            r#"{"session_id":"id","hook_event_name":"SessionEnd","source":"startup"}"#,
            r#"{"session_id":"id","hook_event_name":"SessionStart","source":true}"#,
            r#"{"session_id":"-id","hook_event_name":"SessionStart","source":"startup"}"#,
        ] {
            assert!(parse_hook(AgentKind::Codex, Phase::Start, json).is_err());
        }
    }
}
