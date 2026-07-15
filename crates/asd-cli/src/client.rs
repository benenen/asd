//! UDS connection, handshake, and self-healing daemon startup.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, PROTO_VERSION, paths};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

pub struct Client {
    pub reader: FrameReader<OwnedReadHalf>,
    pub writer: FrameWriter<OwnedWriteHalf>,
    /// The daemon's package version, from the handshake ack.
    pub daemon_version: String,
}

/// Connect + handshake (the client sends Hello first; version mismatches are
/// rejected by the daemon).
pub async fn connect(socket: &Path, kind: ClientKind) -> anyhow::Result<Client> {
    let stream = UnixStream::connect(socket).await.map_err(|e| {
        if matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        ) {
            anyhow::anyhow!(
                "asd-daemon is not running at {} \
                 (start one with `asd new` or `asd attach -A <name>`)",
                socket.display()
            )
        } else {
            anyhow::Error::new(e).context(format!("connecting {}", socket.display()))
        }
    })?;
    let (r, w) = stream.into_split();
    let mut client = Client {
        reader: FrameReader::new(r),
        writer: FrameWriter::new(w),
        daemon_version: String::new(),
    };
    client
        .writer
        .write_frame(&Frame::Hello {
            proto_version: PROTO_VERSION,
            kind,
        })
        .await?;
    match client.reader.read_frame().await? {
        Some(Frame::HelloAck { daemon_version, .. }) => {
            client.daemon_version = daemon_version;
            Ok(client)
        }
        Some(Frame::Error { code, msg }) => bail!("daemon rejected handshake ({code}): {msg}"),
        other => bail!("unexpected handshake reply: {other:?}"),
    }
}

/// Restart the daemon for `socket`: stop the running one (by signal, via the
/// pid file — no handshake, so a `PROTO_VERSION` change can't block it), then
/// start a fresh copy of this binary and wait for it to accept connections.
pub async fn restart(socket: &Path) -> anyhow::Result<Client> {
    stop_daemon(socket).await;
    spawn_daemon(socket)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match connect(socket, ClientKind::Cli).await {
            Ok(c) => return Ok(c),
            Err(e) if Instant::now() >= deadline => {
                return Err(e.context("restarted daemon did not come up within 3s"));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Stop the daemon owning `socket` if one is recorded and alive: SIGTERM it and
/// wait for it to remove its socket (its clean-shutdown signal), escalating to
/// SIGKILL after a grace period; then clear any leftover socket/pid file so a
/// fresh daemon can bind.
async fn stop_daemon(socket: &Path) {
    let pid_path = paths::pid_path(socket);
    if let Some(pid) = std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|&p| p > 0 && process_alive(p))
    {
        // SAFETY: kill(2) with a real signal; failures are ignored (racing exit).
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while socket.exists() {
            if Instant::now() >= deadline {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    // Clean shutdown already removed these; a SIGKILL or crash may not have.
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(&pid_path);
}

/// Whether `pid` exists (a `kill(pid, 0)` probe sends no signal).
fn process_alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs only an existence/permission check.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Self-healing startup (spec §5): connection refused/absent → fork
/// asd-daemon (setsid, logs into the data directory) → retry connecting,
/// capped at 3 seconds.
pub async fn connect_or_spawn(socket: &Path, kind: ClientKind) -> anyhow::Result<Client> {
    match connect(socket, kind).await {
        Ok(c) => return Ok(c),
        Err(_) => spawn_daemon(socket)?,
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match connect(socket, kind).await {
            Ok(c) => return Ok(c),
            Err(e) if Instant::now() >= deadline => {
                return Err(e.context("daemon did not come up within 3s"));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

fn spawn_daemon(socket: &Path) -> anyhow::Result<()> {
    // Single-binary distribution: the daemon is this very executable,
    // re-executed as `asd daemon`
    let exe = std::env::current_exe().context("locating current executable")?;
    let data_dir = paths::data_dir();
    std::fs::create_dir_all(&data_dir).context("creating data dir")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("daemon.log"))
        .context("opening daemon log")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .arg("--socket")
        .arg(socket)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Detach from this terminal's session and process group; the daemon is
    // fully independent of the connection
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().context("spawning asd-daemon")?;
    Ok(())
}
