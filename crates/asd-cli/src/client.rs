//! Daemon connection, handshake, and self-healing daemon startup.
//! Unix: UDS; Windows: named pipe.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use asd_proto::{ClientKind, FrameReader, FrameWriter, paths};
use tokio::io::{AsyncRead, AsyncWrite};

pub struct Client {
    pub reader: FrameReader<Box<dyn AsyncRead + Unpin + Send>>,
    pub writer: FrameWriter<Box<dyn AsyncWrite + Unpin + Send>>,
    /// The daemon's package version, from the handshake ack.
    pub daemon_version: String,
}

/// Connect + handshake (the client sends Hello first; version mismatches are
/// rejected by the daemon).
#[cfg(unix)]
pub async fn connect(socket: &Path, kind: ClientKind) -> anyhow::Result<Client> {
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(socket).await.map_err(|e| {
        if matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        ) {
            anyhow::anyhow!(
                "asd-daemon is not running at {}                  (start one with `asd new` or `asd attach -A <name>`)",
                socket.display()
            )
        } else {
            anyhow::Error::new(e).context(format!("connecting {}", socket.display()))
        }
    })?;
    let (r, w) = stream.into_split();
    let mut client = Client {
        reader: FrameReader::new(Box::new(r)),
        writer: FrameWriter::new(Box::new(w)),
        daemon_version: String::new(),
    };
    handshake(&mut client, kind).await?;
    Ok(client)
}

#[cfg(windows)]
pub async fn connect(socket: &Path, kind: ClientKind) -> anyhow::Result<Client> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let pipe_name = socket.to_str().context("pipe path is not valid UTF-8")?;
    let stream = ClientOptions::new()
        .open(pipe_name)
        .map_err(|e| {
            anyhow::anyhow!(
                "asd-daemon is not running at {}                  (start one with `asd new` or `asd attach -A <name>`)",
                pipe_name
            )
        })?;
    let (r, w) = tokio::io::split(stream);
    let mut client = Client {
        reader: FrameReader::new(Box::new(r)),
        writer: FrameWriter::new(Box::new(w)),
        daemon_version: String::new(),
    };
    handshake(&mut client, kind).await?;
    Ok(client)
}

async fn handshake(client: &mut Client, kind: ClientKind) -> anyhow::Result<()> {
    client.daemon_version = asd_proto::handshake(&mut client.writer, &mut client.reader, kind)
        .await
        .map_err(|msg| anyhow::anyhow!("{msg}"))?;
    Ok(())
}

/// Restart the daemon for `socket`: stop the running one, then start a fresh
/// copy of this binary and wait for it to accept connections.
#[cfg(unix)]
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

#[cfg(windows)]
pub async fn restart(socket: &Path) -> anyhow::Result<Client> {
    // On Windows, named pipes auto-cleanup when the process exits.
    // Just spawn a new daemon — the old one's pipe will be invalid.
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

/// Stop the daemon owning `socket` if one is recorded and alive, for a restart.
#[cfg(unix)]
async fn stop_daemon(socket: &Path) {
    let pid_path = paths::pid_path(socket);
    if let Some(pid) = std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|&p| p > 0 && process_alive(p))
    {
        // SAFETY: kill(2) with a real signal; failures are ignored (racing exit).
        unsafe { libc::kill(pid, libc::SIGUSR1) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while socket.exists() {
            if Instant::now() >= deadline {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(&pid_path);
}

/// Whether `pid` exists (a `kill(pid, 0)` probe sends no signal).
#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Self-healing startup: connection refused/absent → spawn daemon → retry.
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

#[cfg(unix)]
fn spawn_daemon(socket: &Path) -> anyhow::Result<()> {
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

#[cfg(windows)]
fn spawn_daemon(socket: &Path) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locating current executable")?;
    let data_dir = paths::data_dir();
    std::fs::create_dir_all(&data_dir).context("creating data dir")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("daemon.log"))
        .context("opening daemon log")?;

    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .arg("--socket")
        .arg(socket)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .creation_flags(CREATE_NO_WINDOW);
    cmd.spawn().context("spawning asd-daemon")?;
    Ok(())
}
