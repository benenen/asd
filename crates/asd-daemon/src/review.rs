//! Bounded, read-only Git inspection in the daemon's filesystem namespace.
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use asd_proto::{Frame, SessionIdentity, SessionTask};
use tokio::io::{AsyncRead, AsyncReadExt};

const DIFF_LIMIT: usize = 256 * 1024;
const STATUS_LIMIT: usize = 64 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Review output is display text, never a channel for terminal control sequences.
fn safe_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

struct GitOutput {
    text: String,
    truncated: bool,
    success: bool,
    error: String,
}

impl GitOutput {
    fn require_success(self) -> Result<Self, String> {
        if self.success || self.truncated {
            Ok(self)
        } else {
            Err(format!(
                "git review failed: {}",
                safe_text(&self.error).trim()
            ))
        }
    }
}

/// Closing the pipe at the cap bounds memory and stops large Git producers.
async fn bounded_read(
    reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<(String, bool)> {
    let mut bytes = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    let mut truncated = bytes.len() > limit;
    bytes.truncate(limit);
    // Terminal output is text; invalid filename/content bytes are safely replaced.
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        truncated = true;
    }
    Ok((text, truncated))
}

async fn git(directory: &Path, args: &[&str], limit: usize) -> Result<GitOutput, String> {
    git_with_config(directory, args, limit, &[]).await
}

async fn git_with_config(
    directory: &Path,
    args: &[&str],
    limit: usize,
    config: &[String],
) -> Result<GitOutput, String> {
    let mut command = tokio::process::Command::new("git");
    command.arg("--no-pager").args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.untrackedCache=false",
        "-c",
        "color.ui=false",
        "-c",
        "diff.submodule=short",
    ]);
    for setting in config {
        command.arg("-c").arg(setting);
    }
    command
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_EXTERNAL_DIFF",
    ] {
        command.env_remove(key);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot run git: {e}"))?;
    let stdout = child.stdout.take().ok_or("git stdout is unavailable")?;
    let stderr = child.stderr.take().ok_or("git stderr is unavailable")?;
    let read = async {
        let (out, err, status) = tokio::try_join!(
            bounded_read(stdout, limit),
            bounded_read(stderr, 8192),
            child.wait()
        )?;
        Ok::<_, std::io::Error>(GitOutput {
            text: out.0,
            truncated: out.1,
            error: err.0,
            success: status.success(),
        })
    };
    tokio::time::timeout(COMMAND_TIMEOUT, read)
        .await
        .map_err(|_| "git review timed out".to_string())?
        .map_err(|e| format!("cannot read git result: {e}"))
}

/// Attributes can run clean/process filters even for ordinary diff/status.
/// Enumerate configured drivers, then explicitly disable them for inspection.
async fn disabled_filters(directory: &Path) -> Result<Vec<String>, String> {
    let keys = git(
        directory,
        &[
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            "^filter\\.",
        ],
        8192,
    )
    .await?;
    if keys.truncated || keys.text.contains('\u{fffd}') {
        return Err("Git filter configuration exceeds safe review limits".into());
    }
    if !keys.success && (!keys.text.is_empty() || !keys.error.is_empty()) {
        return Err(format!(
            "cannot inspect Git filter configuration: {}",
            safe_text(&keys.error)
        ));
    }
    let drivers: std::collections::BTreeSet<_> = keys
        .text
        .split('\0')
        .filter_map(|key| {
            key.strip_prefix("filter.")?
                .rsplit_once('.')
                .map(|(driver, _)| driver)
        })
        .collect();
    let settings: Vec<_> = drivers
        .into_iter()
        .flat_map(|driver| {
            [
                format!("filter.{driver}.clean="),
                format!("filter.{driver}.process="),
                format!("filter.{driver}.required=false"),
            ]
        })
        .collect();
    if settings
        .iter()
        .map(|setting| setting.len() + 4)
        .sum::<usize>()
        > 16 * 1024
    {
        return Err("Git filter configuration exceeds safe review limits".into());
    }
    Ok(settings)
}

pub(crate) async fn collect(
    identity: SessionIdentity,
    task: Option<SessionTask>,
    directory: PathBuf,
) -> Result<Frame, String> {
    let root = git(&directory, &["rev-parse", "--show-toplevel"], 8192)
        .await?
        .require_success()?;
    if root.truncated {
        return Err("repository directory exceeds review limit".into());
    }
    // Git prints one trailing newline; preserve other whitespace in directory names.
    let root_path = PathBuf::from(
        root.text
            .strip_suffix('\n')
            .unwrap_or(&root.text)
            .trim_end_matches('\r'),
    );
    let directory = root_path
        .canonicalize()
        .map_err(|e| format!("cannot resolve repository directory: {e}"))?;
    let directory_text = directory
        .to_str()
        .ok_or("repository directory is not valid UTF-8")?
        .to_owned();
    let head = git(
        &directory,
        &["rev-parse", "--verify", "--short", "HEAD"],
        8192,
    )
    .await?;
    let branch = git(
        &directory,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        8192,
    )
    .await?;
    let branch = if branch.success {
        format!(
            "{}{}",
            branch.text.trim(),
            if head.success { "" } else { " (unborn)" }
        )
    } else if head.success {
        format!("detached at {}", head.text.trim())
    } else {
        return Err("cannot determine repository HEAD".into());
    };
    let filters = disabled_filters(&directory).await?;
    let status = git_with_config(
        &directory,
        &[
            "status",
            "--short",
            "--untracked-files=all",
            "--ignore-submodules=dirty",
        ],
        STATUS_LIMIT,
        &filters,
    )
    .await?
    .require_success()?;
    let staged = git_with_config(
        &directory,
        &[
            "diff",
            "--cached",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--ignore-submodules=dirty",
            "--",
        ],
        DIFF_LIMIT,
        &filters,
    )
    .await?
    .require_success()?;
    let unstaged = git_with_config(
        &directory,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--ignore-submodules=dirty",
            "--",
        ],
        DIFF_LIMIT,
        &filters,
    )
    .await?
    .require_success()?;
    let diff = [
        ("Staged changes", &staged.text),
        ("Unstaged changes", &unstaged.text),
    ]
    .into_iter()
    .filter(|(_, text)| !text.is_empty())
    .map(|(label, text)| format!("{label}\n{}", safe_text(text)))
    .collect::<Vec<_>>()
    .join("\n");
    Ok(Frame::SessionReview {
        identity,
        task,
        directory: directory_text,
        branch: safe_text(&branch),
        status: safe_text(&status.text),
        diff,
        truncated: status.truncated || staged.truncated || unstaged.truncated,
    })
}

#[cfg(test)]
mod tests;
