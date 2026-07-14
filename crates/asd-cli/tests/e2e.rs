//! Integration tests (spec §8): real UDS + real daemon process + the `asd` CLI.
//!
//! Coverage: create → write → attach asserting the snapshot → detach →
//! re-attach asserting the accumulated output; multi-client broadcast;
//! version-mismatch rejection; --stdio proxy passthrough; no leftover child
//! processes and socket cleanup after daemon SIGTERM.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, PROTO_VERSION, code};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::time::timeout;

const TICK: Duration = Duration::from_millis(50);
const WAIT: Duration = Duration::from_secs(10);

fn cli_exe() -> &'static str {
    env!("CARGO_BIN_EXE_asd")
}

/// An isolated daemon instance: its own socket + data directory, reclaimed
/// on Drop.
struct Daemon {
    child: Child,
    socket: PathBuf,
    dir: PathBuf,
}

impl Daemon {
    fn start(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "asd-e2e-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("asd.sock");
        let child = Command::new(cli_exe())
            .arg("daemon")
            .arg("--socket")
            .arg(&socket)
            .env("XDG_DATA_HOME", dir.join("data"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn asd daemon");

        // Wait until the socket is connectable
        let deadline = std::time::Instant::now() + WAIT;
        while !socket.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "daemon socket never appeared"
            );
            std::thread::sleep(TICK);
        }
        Self { child, socket, dir }
    }

    fn cli(&self) -> Command {
        let mut cmd = Command::new(cli_exe());
        cmd.arg("--socket").arg(&self.socket);
        cmd
    }

    /// Pids of the daemon's direct children (each session's shell), scanned
    /// from /proc.
    fn child_pids(&self) -> Vec<u32> {
        let daemon_pid = self.child.id();
        let mut pids = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            // stat format: `pid (comm) state ppid ...`; comm may contain
            // spaces/parentheses, so take fields after the last ')'
            if let Some(idx) = stat.rfind(')')
                && let Some(ppid) = stat[idx + 1..].split_whitespace().nth(1)
                && ppid == daemon_pid.to_string()
            {
                pids.push(pid);
            }
        }
        pids
    }

    fn sigterm(&self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Direct protocol client (simulating the GUI/CLI data plane).
struct ProtoClient {
    reader: FrameReader<tokio::net::unix::OwnedReadHalf>,
    writer: FrameWriter<tokio::net::unix::OwnedWriteHalf>,
}

impl ProtoClient {
    async fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).await.expect("connect failed");
        let (r, w) = stream.into_split();
        let mut c = Self {
            reader: FrameReader::new(r),
            writer: FrameWriter::new(w),
        };
        c.send(Frame::Hello {
            proto_version: PROTO_VERSION,
            kind: ClientKind::Cli,
        })
        .await;
        match c.recv().await {
            Frame::HelloAck { proto_version, .. } => assert_eq!(proto_version, PROTO_VERSION),
            other => panic!("expected HelloAck, got {other:?}"),
        }
        c
    }

    async fn send(&mut self, frame: Frame) {
        timeout(WAIT, self.writer.write_frame(&frame))
            .await
            .expect("write timeout")
            .expect("write failed");
    }

    async fn recv(&mut self) -> Frame {
        timeout(WAIT, self.reader.read_frame())
            .await
            .expect("read timeout")
            .expect("read failed")
            .expect("connection closed unexpectedly")
    }

    /// Attach and return the Snapshot contents.
    async fn attach(&mut self, name: &str) -> Vec<u8> {
        self.send(Frame::Attach {
            name: name.into(),
            cols: 80,
            rows: 24,
        })
        .await;
        match self.recv().await {
            Frame::Snapshot { vt } => vt,
            other => panic!("expected Snapshot, got {other:?}"),
        }
    }

    /// Keep receiving Output until needle appears in the accumulated bytes.
    async fn read_output_until(&mut self, needle: &[u8]) -> Vec<u8> {
        let mut acc = Vec::new();
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "needle {:?} not seen in output: {:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&acc)
            );
            match self.recv().await {
                Frame::Output { bytes } => {
                    acc.extend_from_slice(&bytes);
                    if acc.windows(needle.len()).any(|w| w == needle) {
                        return acc;
                    }
                }
                other => panic!("expected Output, got {other:?}"),
            }
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---- Tests ----

/// M0 acceptance core flow: create → write → assert output → detach →
/// re-attach and assert the snapshot contains the accumulated output.
#[tokio::test]
async fn create_write_detach_reattach_preserves_state() {
    let daemon = Daemon::start("core");

    // CLI create
    let out = daemon.cli().args(["new", "work"]).output().unwrap();
    assert!(out.status.success(), "create failed: {out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "work");

    // Client A attaches and writes
    let mut a = ProtoClient::connect(&daemon.socket).await;
    let _snapshot = a.attach("work").await;
    a.send(Frame::Input {
        bytes: b"echo marker-$((40+2))\n".to_vec(),
    })
    .await;
    // The result of executing the echo (not the echo-back — that contains
    // the literal expression)
    a.read_output_until(b"marker-42").await;

    // Dropping the connection means detach (spec §5)
    drop(a);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Client B re-attaches: the snapshot must restore the accumulated output
    let mut b = ProtoClient::connect(&daemon.socket).await;
    let snapshot = b.attach("work").await;
    assert!(
        contains(&snapshot, b"marker-42"),
        "snapshot must contain prior output, got: {:?}",
        String::from_utf8_lossy(&snapshot)
    );
}

/// GUI and CLI attached to the same session simultaneously see identical
/// output (M0 acceptance item 3).
#[tokio::test]
async fn two_clients_both_receive_broadcast() {
    let daemon = Daemon::start("broadcast");
    let out = daemon.cli().args(["new", "dual"]).output().unwrap();
    assert!(out.status.success());

    let mut a = ProtoClient::connect(&daemon.socket).await;
    let mut b = ProtoClient::connect(&daemon.socket).await;
    a.attach("dual").await;
    b.attach("dual").await;

    a.send(Frame::Input {
        bytes: b"echo dual-$((50+7))\n".to_vec(),
    })
    .await;
    a.read_output_until(b"dual-57").await;
    b.read_output_until(b"dual-57").await;
}

/// The list/kill CLI surface + session lifecycle.
#[tokio::test]
async fn list_and_kill_via_cli() {
    let daemon = Daemon::start("listkill");

    let out = daemon.cli().args(["new", "tokill"]).output().unwrap();
    assert!(out.status.success());

    let out = daemon.cli().arg("list").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("tokill"), "list output: {stdout}");

    let out = daemon.cli().args(["kill", "tokill"]).output().unwrap();
    assert!(out.status.success(), "kill failed: {out:?}");

    // End-to-end session death is asynchronous (SIGHUP → EOF → reap)
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let out = daemon.cli().arg("list").output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.contains("tokill") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session survived kill: {stdout}"
        );
        std::thread::sleep(TICK);
    }

    // Killing a nonexistent session errors
    let out = daemon.cli().args(["kill", "nope"]).output().unwrap();
    assert!(!out.status.success());
}

/// Version mismatch: the daemon replies Error{code=1} then disconnects
/// (spec §4).
#[tokio::test]
async fn version_mismatch_is_rejected() {
    let daemon = Daemon::start("vermatch");
    let stream = UnixStream::connect(&daemon.socket).await.unwrap();
    let (r, w) = stream.into_split();
    let mut reader = FrameReader::new(r);
    let mut writer = FrameWriter::new(w);

    writer
        .write_frame(&Frame::Hello {
            proto_version: PROTO_VERSION + 1,
            kind: ClientKind::Cli,
        })
        .await
        .unwrap();
    match timeout(WAIT, reader.read_frame()).await.unwrap().unwrap() {
        Some(Frame::Error { code: c, .. }) => assert_eq!(c, code::VERSION_MISMATCH),
        other => panic!("expected version-mismatch Error, got {other:?}"),
    }
    // Followed by disconnect
    assert!(matches!(
        timeout(WAIT, reader.read_frame()).await.unwrap(),
        Ok(None) | Err(_)
    ));
}

/// `asd attach --stdio`: stdio ↔ UDS passthrough; protocol frames traverse
/// the pipe unchanged.
#[tokio::test]
async fn stdio_proxy_passes_protocol_through() {
    let daemon = Daemon::start("stdio");
    let out = daemon.cli().args(["new", "via-proxy"]).output().unwrap();
    assert!(out.status.success());

    let mut proxy = tokio::process::Command::new(cli_exe())
        .arg("--socket")
        .arg(&daemon.socket)
        .args(["attach", "via-proxy", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let stdin = proxy.stdin.take().unwrap();
    let stdout = proxy.stdout.take().unwrap();
    let mut writer = FrameWriter::new(stdin);
    let mut reader = FrameReader::new(stdout);

    write_read_handshake(&mut writer, &mut reader).await;
    writer.write_frame(&Frame::ListSessions).await.unwrap();
    match timeout(WAIT, reader.read_frame()).await.unwrap().unwrap() {
        Some(Frame::SessionList { sessions }) => {
            assert!(sessions.iter().any(|s| s.name == "via-proxy"));
        }
        other => panic!("expected SessionList via proxy, got {other:?}"),
    }
    let _ = proxy.kill().await;
}

async fn write_read_handshake<W, R>(writer: &mut FrameWriter<W>, reader: &mut FrameReader<R>)
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    writer
        .write_frame(&Frame::Hello {
            proto_version: PROTO_VERSION,
            kind: ClientKind::Proxy,
        })
        .await
        .unwrap();
    match timeout(WAIT, reader.read_frame()).await.unwrap().unwrap() {
        Some(Frame::HelloAck { .. }) => {}
        other => panic!("expected HelloAck, got {other:?}"),
    }
}

/// Daemon SIGTERM: children exit cleanly and the socket is cleaned up
/// (M0 acceptance item 4).
#[tokio::test]
async fn sigterm_reaps_children_and_removes_socket() {
    let mut daemon = Daemon::start("sigterm");
    let out = daemon
        .cli()
        .args(["new", "longrun", "--cmd", "sleep 300"])
        .output()
        .unwrap();
    assert!(out.status.success());

    // Wait for the session's child process to appear
    let deadline = std::time::Instant::now() + WAIT;
    let pids = loop {
        let pids = daemon.child_pids();
        if !pids.is_empty() {
            break pids;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no session child spawned"
        );
        std::thread::sleep(TICK);
    };

    daemon.sigterm();

    // The daemon exits (shutdown contract capped at a 2s grace period, plus
    // margin); note the daemon is a child of this process, so it must be
    // reaped via try_wait rather than probed with kill(pid,0)
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        if daemon.child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "daemon did not exit");
        std::thread::sleep(TICK);
    }

    // No leftover children
    for pid in pids {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
        let alive = matches!(&stat, Some(s) if !s.contains(" Z "));
        assert!(
            !alive,
            "session child {pid} survived daemon SIGTERM: {stat:?}"
        );
    }
    // The socket has been cleaned up
    assert!(!daemon.socket.exists(), "socket file not removed");
}
