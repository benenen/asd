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
    };
    client
        .writer
        .write_frame(&Frame::Hello {
            proto_version: PROTO_VERSION,
            kind,
        })
        .await?;
    match client.reader.read_frame().await? {
        Some(Frame::HelloAck { .. }) => Ok(client),
        Some(Frame::Error { code, msg }) => bail!("daemon rejected handshake ({code}): {msg}"),
        other => bail!("unexpected handshake reply: {other:?}"),
    }
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
