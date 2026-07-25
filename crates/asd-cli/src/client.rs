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
    let stream = ClientOptions::new().open(pipe_name).map_err(|e| {
        // Only "no such pipe" means no daemon. Everything else — the pipe is
        // busy because every instance is taken, access denied, … — must keep
        // its real error, or the user chases a daemon that is in fact running.
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "asd-daemon is not running at {pipe_name} \
                 (start one with `asd new` or `asd attach -A <name>`)"
            )
        } else {
            anyhow::Error::new(e).context(format!("connecting {pipe_name}"))
        }
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
    client.daemon_version = asd_client::handshake(&mut client.writer, &mut client.reader, kind)
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
    // The old daemon must actually be stopped first. A named pipe lives as long
    // as its owning process, so "just spawn a new one" leaves the old daemon
    // holding the name — and since the successor claims it with
    // `first_pipe_instance(true)`, the new daemon would fail to start at all.
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

/// Stop the daemon owning `socket` if one is recorded, for a restart.
///
/// Windows has no signals, so the recorded pid is ended with TerminateProcess.
/// That is abrupt — the daemon does not get to refresh each session's live cwd
/// on the way out — but the session list on disk is rewritten on every
/// create/rename/kill, so a restart still restores every session at its last
/// recorded cwd. That is the same guarantee a SIGKILLed unix daemon gives.
#[cfg(windows)]
async fn stop_daemon(socket: &Path) {
    let pid_path = paths::pid_path(socket);
    if let Some(pid) = std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&p| p > 0)
    {
        win32::terminate(pid);
        // The successor claims the pipe name with `first_pipe_instance(true)`
        // and would fail outright if the old one still held it, so wait for the
        // name to be released before spawning.
        let deadline = Instant::now() + Duration::from_secs(3);
        while pipe_in_use(socket) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let _ = std::fs::remove_file(&pid_path);
}

/// Whether any process still serves the named pipe. Anything other than "no
/// such pipe" — all instances busy, access denied — still means it exists.
#[cfg(windows)]
fn pipe_in_use(socket: &Path) -> bool {
    use tokio::net::windows::named_pipe::ClientOptions;
    let Some(name) = socket.to_str() else {
        return false;
    };
    match ClientOptions::new().open(name) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Minimal kernel32 process control (no extra crate, mirrors asd-daemon's).
#[cfg(windows)]
mod win32 {
    unsafe extern "system" {
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> isize;
        fn TerminateProcess(hProcess: isize, uExitCode: u32) -> i32;
        fn CloseHandle(hObject: isize) -> i32;
    }

    const PROCESS_TERMINATE: u32 = 0x0001;

    /// End `pid`. Failure means it is already gone (or not ours to kill), which
    /// for a restart is indistinguishable from success.
    pub fn terminate(pid: u32) {
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        if handle != 0 {
            unsafe {
                TerminateProcess(handle, 0);
                CloseHandle(handle);
            }
        }
    }
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
