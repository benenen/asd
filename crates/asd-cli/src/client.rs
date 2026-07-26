//! Daemon connection, handshake, and self-healing daemon startup.
//!
//! The transport differs per platform (UDS on unix, named pipe on Windows) and
//! so does detaching a spawned daemon; both live in [`crate::platform`], so
//! everything here is written once.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use asd_proto::{ClientKind, FrameReader, FrameWriter, paths};

use crate::platform::{self, BoxRead, BoxWrite};

pub struct Client {
    pub reader: FrameReader<BoxRead>,
    pub writer: FrameWriter<BoxWrite>,
    /// The daemon's package version, from the handshake ack.
    pub daemon_version: String,
}

/// Connect + handshake (the client sends Hello first; version mismatches are
/// rejected by the daemon).
pub async fn connect(socket: &Path, kind: ClientKind) -> anyhow::Result<Client> {
    let (r, w) = platform::connect_stream(socket).await?;
    let mut client = Client {
        reader: FrameReader::new(r),
        writer: FrameWriter::new(w),
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
///
/// Stopping first is not optional on either platform: unix would rebind onto a
/// socket the old daemon still owns, and on Windows a named pipe lives as long
/// as its owning process, so the successor — which claims the name exclusively —
/// would fail to start at all.
pub async fn restart(socket: &Path) -> anyhow::Result<Client> {
    platform::stop_daemon(socket).await;
    spawn_daemon(socket)?;
    await_daemon(socket, "restarted daemon did not come up within 3s").await
}

/// Self-healing startup: connection refused/absent → spawn daemon → retry.
pub async fn connect_or_spawn(socket: &Path, kind: ClientKind) -> anyhow::Result<Client> {
    match connect(socket, kind).await {
        Ok(c) => return Ok(c),
        Err(_) => spawn_daemon(socket)?,
    }
    await_daemon(socket, "daemon did not come up within 3s").await
}

/// Poll until the daemon accepts a connection, or give up after 3s.
async fn await_daemon(socket: &Path, timeout_msg: &'static str) -> anyhow::Result<Client> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match connect(socket, ClientKind::Cli).await {
            Ok(c) => return Ok(c),
            Err(e) if Instant::now() >= deadline => return Err(e.context(timeout_msg)),
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Start a detached daemon for `socket`, logging into the data dir.
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
    platform::configure_detached(&mut cmd);
    cmd.spawn().context("spawning asd-daemon")?;
    Ok(())
}
