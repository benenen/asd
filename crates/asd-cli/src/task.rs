//! Durable task associations and daemon-side, read-only Git review.
use std::path::Path;

use anyhow::{Context, bail};
use asd_proto::{ClientKind, Frame, SessionInfo, SessionTask};
use clap::Args;

use crate::{client, exit};

#[derive(Args, Debug)]
pub struct TaskArgs {
    /// Session name. Omit inside a session to use its rename-stable identity.
    pub name: Option<String>,
    /// Durable task description (requires --directory).
    #[arg(long, requires = "directory", conflicts_with = "clear")]
    pub description: Option<String>,
    /// Absolute directory on the daemon's machine (requires --description).
    #[arg(long, requires = "description", conflicts_with = "clear")]
    pub directory: Option<String>,
    /// Remove the task association without changing files or the running agent.
    #[arg(long)]
    pub clear: bool,
    /// Print task metadata as JSON; null means no association.
    #[arg(long)]
    pub json: bool,
}

async fn resolve(c: &mut client::Client, name: Option<String>) -> anyhow::Result<SessionInfo> {
    let own = if name.is_none() {
        Some(
            std::env::var("ASD_SESSION_ID")
                .context("provide a session name outside an asd session")?
                .parse::<asd_proto::SessionIdentity>()
                .map_err(|e| anyhow::anyhow!("invalid ASD_SESSION_ID: {e}"))?,
        )
    } else {
        None
    };
    c.writer.write_frame(&Frame::ListSessions).await?;
    let sessions = match c.reader.read_frame().await? {
        Some(Frame::SessionList { sessions }) => sessions,
        Some(Frame::Error { code, msg }) => return Err(exit::daemon("task", code, &msg)),
        other => bail!("unexpected session list: {other:?}"),
    };
    sessions
        .into_iter()
        .find(|s| match own {
            Some(identity) => s.identity() == identity,
            None => name.as_deref() == Some(s.name.as_str()),
        })
        .ok_or_else(|| {
            exit::daemon(
                "task",
                asd_proto::code::NO_SUCH_SESSION,
                "session no longer exists",
            )
        })
}

pub async fn task(socket: &Path, args: TaskArgs) -> anyhow::Result<()> {
    if let (Some(description), Some(directory)) = (&args.description, &args.directory) {
        SessionTask {
            description: description.clone(),
            directory: directory.clone(),
        }
        .validate()
        .map_err(|error| anyhow::anyhow!("task: {error}"))?;
    }
    let mut c = client::connect(socket, ClientKind::Cli).await?;
    let info = resolve(&mut c, args.name).await?;
    let setting = args.clear || args.description.is_some();
    let task = if setting {
        match (args.description, args.directory) {
            (Some(description), Some(directory)) => Some(SessionTask {
                description,
                directory,
            }),
            _ if args.clear => None,
            _ => bail!("task: --description and --directory must be supplied together"),
        }
    } else {
        info.task.clone()
    };
    if setting {
        c.writer
            .write_frame(&Frame::SetSessionTask {
                identity: info.identity(),
                task,
            })
            .await?;
        match c.reader.read_frame().await? {
            Some(Frame::Ack) => {}
            Some(Frame::Error { code, msg }) => return Err(exit::daemon("task", code, &msg)),
            other => bail!("unexpected task reply: {other:?}"),
        }
        // Read the accepted canonical association; never report the unvalidated input.
        c.writer.write_frame(&Frame::ListSessions).await?;
        let Some(Frame::SessionList { sessions }) = c.reader.read_frame().await? else {
            bail!("task: failed to read accepted association");
        };
        let current = sessions
            .iter()
            .find(|s| s.identity() == info.identity())
            .ok_or_else(|| {
                exit::daemon(
                    "task",
                    asd_proto::code::NO_SUCH_SESSION,
                    "session ended after update",
                )
            })?;
        print_task(current.task.as_ref(), args.json)?;
    } else {
        print_task(task.as_ref(), args.json)?;
    }
    Ok(())
}

fn print_task(task: Option<&SessionTask>, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string(&task)?);
    } else if let Some(task) = task {
        println!(
            "task       {}\ndirectory  {}",
            plain(&task.description),
            plain(&task.directory)
        );
    } else {
        println!("No task associated. Set --description and --directory to link this session.");
    }
    Ok(())
}

/// Keep repository text from issuing terminal controls in plain CLI output.
fn plain(text: &str) -> String {
    text.chars()
        .filter(|c| {
            (!c.is_control() || matches!(c, '\n' | '\t'))
                && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

pub async fn review(socket: &Path, name: Option<String>, json: bool) -> anyhow::Result<()> {
    let mut c = client::connect(socket, ClientKind::Cli).await?;
    let info = resolve(&mut c, name).await?;
    c.writer
        .write_frame(&Frame::GetSessionReview {
            identity: info.identity(),
        })
        .await?;
    let reply = tokio::time::timeout(std::time::Duration::from_secs(30), c.reader.read_frame())
        .await
        .context("review: daemon did not reply within 30 seconds")??;
    match reply {
        Some(Frame::SessionReview {
            identity,
            task,
            directory,
            branch,
            status,
            diff,
            truncated,
        }) if identity == info.identity() => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"session":info.name,"identity":identity.to_string(),"task":task,"directory":directory,"branch":branch,"status":status,"diff":diff,"truncated":truncated})
                );
            } else {
                println!("session    {}", plain(&info.name));
                if let Some(task) = task {
                    println!("task       {}", plain(&task.description));
                }
                println!(
                    "worktree   {}\nbranch     {}\n\n{}\n{}",
                    plain(&directory),
                    plain(&branch),
                    plain(&status),
                    plain(&diff)
                );
                println!(
                    "Submodule working-file changes require a separate review in the submodule directory."
                );
                if truncated {
                    println!("[Review truncated; inspect the remaining changes in the worktree.]");
                }
            }
        }
        Some(Frame::Error { code, msg }) => return Err(exit::daemon("review", code, &msg)),
        other => bail!("unexpected review reply: {other:?}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn task_parser_requires_complete_association_and_allows_clear() {
        assert!(
            crate::Args::try_parse_from(["asd", "task", "s0", "--description", "fix"]).is_err()
        );
        assert!(
            crate::Args::try_parse_from(["asd", "task", "s0", "--directory", "/repo"]).is_err()
        );
        assert!(
            crate::Args::try_parse_from([
                "asd",
                "task",
                "s0",
                "--clear",
                "--description",
                "fix",
                "--directory",
                "/repo"
            ])
            .is_err()
        );
        assert!(
            crate::Args::try_parse_from([
                "asd",
                "task",
                "s0",
                "--description",
                "修复",
                "--directory",
                "/repo"
            ])
            .is_ok()
        );
        assert!(crate::Args::try_parse_from(["asd", "task", "--clear"]).is_ok());
    }

    #[test]
    fn review_text_cannot_ring_bell_or_reorder_labels() {
        assert_eq!(plain("+safe\x07\x1b\u{202e}\n\tline"), "+safe\n\tline");
    }
}
