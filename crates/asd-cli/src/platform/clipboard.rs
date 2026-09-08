//! Bounded subprocess transport for native clipboard helpers.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) const MAX_IMAGE_BYTES: usize = 1024 * 1024;
const HELPER_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn run_helper(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    max_output: usize,
) -> anyhow::Result<Vec<u8>> {
    run_helper_with_timeout(program, args, input, max_output, HELPER_TIMEOUT)
}

fn run_helper_with_timeout(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    max_output: usize,
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    // The CLI already owns a runtime. A joined worker keeps these synchronous
    // platform APIs safe there without leaving a background executor alive.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("starting clipboard helper runtime")?
                    .block_on(run_async(program, args, input, max_output, timeout))
            })
            .join()
            .map_err(|_| anyhow::anyhow!("clipboard helper worker failed"))?
    })
}

async fn run_async(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    max_output: usize,
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        // Clipboard ownership helpers may legitimately detach. Do not wait
        // for their long-lived owner to close an inherited stdout pipe.
        .stdout(if max_output > 0 { Stdio::piped() } else { Stdio::null() })
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot start clipboard helper {program}; install it and use a local desktop session"))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let operation = async {
        let write = async {
            if let (Some(mut stdin), Some(input)) = (stdin, input) {
                stdin
                    .write_all(input)
                    .await
                    .context("writing clipboard helper input")?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let read = async {
            let mut bytes = Vec::new();
            if let Some(stdout) = stdout {
                stdout
                    .take(max_output as u64 + 1)
                    .read_to_end(&mut bytes)
                    .await
                    .context("reading clipboard helper output")?;
            }
            if bytes.len() > max_output {
                bail!("clipboard image exceeds the 1 MiB limit");
            }
            Ok::<_, anyhow::Error>(bytes)
        };
        let wait = async { child.wait().await.context("waiting for clipboard helper") };
        let ((), bytes, status) = tokio::try_join!(write, read, wait)?;
        if !status.success() {
            bail!(
                "clipboard helper {program} failed; check that the desktop clipboard contains an image and the local desktop is available"
            );
        }
        Ok(bytes)
    };
    let result = tokio::time::timeout(timeout, operation)
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("clipboard helper {program} timed out")));
    if result.is_err() {
        // Only this invocation's child is terminated. No existing clipboard
        // owner or desktop process is touched.
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn helper_preserves_stdin_bytes_without_shell_interpolation() {
        let input = b"'\"$(never-run)\nlast line";
        let bytes = run_helper("/bin/cat", &[], Some(input), 128).unwrap();
        assert_eq!(bytes, input);
    }

    #[test]
    fn helper_rejects_output_larger_than_limit() {
        let error = run_helper("/bin/cat", &[], Some(b"12345"), 4).unwrap_err();
        assert!(error.to_string().contains("limit"));
        assert_eq!(
            run_helper("/bin/cat", &[], Some(b"1234"), 4).unwrap(),
            b"1234"
        );
    }

    #[test]
    fn helper_times_out_and_reaps_its_child() {
        let started = std::time::Instant::now();
        let error = run_helper_with_timeout(
            "/bin/sh",
            &["-c", "exec sleep 30"],
            None,
            128,
            Duration::from_millis(40),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn helper_reports_missing_executable_and_failed_exit_without_stderr() {
        assert!(
            run_helper("/asd-missing-clipboard-helper", &[], None, 8)
                .unwrap_err()
                .to_string()
                .contains("install")
        );
        let error = run_helper(
            "/bin/sh",
            &["-c", "echo private-content >&2; exit 1"],
            None,
            8,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("failed"));
        assert!(!error.contains("private-content"));
    }

    #[tokio::test]
    async fn synchronous_helper_can_run_inside_cli_runtime() {
        assert_eq!(
            run_helper("/bin/cat", &[], Some(b"hello"), 8).unwrap(),
            b"hello"
        );
    }
}
