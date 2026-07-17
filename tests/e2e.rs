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

    /// Receive the next frame that is not Output (draining live Output).
    async fn recv_skipping_output(&mut self) -> Frame {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected a non-Output frame within the deadline"
            );
            match self.recv().await {
                Frame::Output { .. } => {}
                other => return other,
            }
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

    let out = daemon
        .cli()
        .args(["new", "tokill", "--cmd", "sleep 300"])
        .output()
        .unwrap();
    assert!(out.status.success());

    let out = daemon.cli().arg("list").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("tokill"), "list output: {stdout}");
    // The command (SessionInfo.command, proto v2) reaches the client.
    assert!(stdout.contains("sleep 300"), "list output: {stdout}");

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

/// `asd restart` stops the running daemon (by signal, via the pid file) and
/// brings up a fresh one; sessions are dropped. This is the recovery path for a
/// protocol-version bump, where the client can't handshake the old daemon.
#[tokio::test]
async fn restart_replaces_the_daemon() {
    let mut daemon = Daemon::start("restart");
    let old_pid = daemon.child.id();

    // A session to lose across the restart.
    assert!(
        daemon
            .cli()
            .args(["new", "gone"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let out = daemon.cli().arg("restart").output().unwrap();
    assert!(out.status.success(), "restart failed: {out:?}");

    // The old daemon exited — reap the zombie child.
    let deadline = std::time::Instant::now() + WAIT;
    while daemon.child.try_wait().unwrap().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "old daemon survived restart"
        );
        std::thread::sleep(TICK);
    }

    // A fresh daemon is up under a new pid, answers `list`, and the old session
    // did not survive.
    let new_pid: i32 = std::fs::read_to_string(daemon.socket.with_extension("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_ne!(new_pid as u32, old_pid, "restart reused the old pid");
    let list = daemon.cli().arg("list").output().unwrap();
    assert!(list.status.success(), "list after restart failed: {list:?}");
    assert!(
        !String::from_utf8_lossy(&list.stdout).contains("gone"),
        "session survived restart"
    );

    // The fresh daemon is detached (not our child); stop it so it doesn't leak.
    unsafe { libc::kill(new_pid, libc::SIGTERM) };
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

/// M1 scrollback: write more than a screen of lines, then FetchHistory must
/// return the earlier lines that scrolled off (spec §4).
#[tokio::test]
async fn fetch_history_returns_scrolled_off_lines() {
    let daemon = Daemon::start("history");
    let out = daemon.cli().args(["new", "hist"]).output().unwrap();
    assert!(out.status.success());

    let mut a = ProtoClient::connect(&daemon.socket).await;
    a.attach("hist").await;
    // Print 60 numbered lines into a 24-row screen: the first ~36 scroll off.
    a.send(Frame::Input {
        bytes: b"for i in $(seq 1 60); do echo HL-$i; done\n".to_vec(),
    })
    .await;
    // Wait until the last line has been produced so scrollback is populated.
    a.read_output_until(b"HL-60").await;

    // Fetch the whole screen space; earliest lines must be present.
    a.send(Frame::FetchHistory {
        start: 0,
        count: 4000,
    })
    .await;
    let (total_rows, rows) = match a.recv_skipping_output().await {
        Frame::History {
            total_rows, rows, ..
        } => (total_rows, rows),
        other => panic!("expected History, got {other:?}"),
    };
    assert!(total_rows > 24, "scrollback should exceed one screen");
    let flat: Vec<String> = rows
        .iter()
        .map(|r| String::from_utf8_lossy(r).trim_end().to_string())
        .collect();
    // A line that must have scrolled off the 24-row live screen.
    assert!(
        flat.iter().any(|l| l == "HL-1"),
        "earliest scrolled-off line missing from history: {flat:?}"
    );
    assert!(
        flat.iter().any(|l| l == "HL-60"),
        "latest line missing from history"
    );

    // A narrow window near the top returns just those rows.
    a.send(Frame::FetchHistory { start: 0, count: 3 }).await;
    match a.recv_skipping_output().await {
        Frame::History { rows, start, .. } => {
            assert_eq!(start, 0);
            assert_eq!(rows.len(), 3);
        }
        other => panic!("expected History, got {other:?}"),
    }
}

/// Refresh returns a fresh Snapshot of the live screen (used to resync after
/// leaving the client-side scrollback view).
#[tokio::test]
async fn refresh_returns_fresh_snapshot() {
    let daemon = Daemon::start("refresh");
    let out = daemon.cli().args(["new", "refr"]).output().unwrap();
    assert!(out.status.success());

    let mut a = ProtoClient::connect(&daemon.socket).await;
    a.attach("refr").await;
    a.send(Frame::Input {
        bytes: b"echo REFRESH-MARK\n".to_vec(),
    })
    .await;
    a.read_output_until(b"REFRESH-MARK").await;

    a.send(Frame::Refresh).await;
    match a.recv_skipping_output().await {
        Frame::Snapshot { vt } => {
            assert!(
                contains(&vt, b"REFRESH-MARK"),
                "refresh snapshot missing recent output: {:?}",
                String::from_utf8_lossy(&vt)
            );
        }
        other => panic!("expected Snapshot from Refresh, got {other:?}"),
    }
}

/// v4 scripting: `send` types into a session (bytes reach the pty and run),
/// `wait --text` blocks until the rendered screen matches, and `peek` prints
/// that screen — all attach-free, over the CLI.
#[tokio::test]
async fn send_wait_peek_round_trip() {
    let daemon = Daemon::start("sendpeek");
    assert!(
        daemon
            .cli()
            .args(["new", "work"])
            .output()
            .unwrap()
            .status
            .success()
    );

    // The marker lives only in the command's *output*, not the echoed command
    // line ($((6*7)) is typed, 42 only appears once the pty runs it) — so a
    // match proves `send` delivered the bytes and the trailing Enter.
    let out = daemon
        .cli()
        .args([
            "send",
            "work",
            "--text",
            "echo sendmark-$((6*7))",
            "--enter",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "send failed: {out:?}");

    // wait --text polls peek until the screen contains the output.
    let out = daemon
        .cli()
        .args(["wait", "work", "--text", "sendmark-42", "--timeout", "10s"])
        .output()
        .unwrap();
    assert!(out.status.success(), "wait --text failed: {out:?}");

    // peek prints the rendered screen, which now carries the marker.
    let out = daemon.cli().args(["peek", "work"]).output().unwrap();
    assert!(out.status.success(), "peek failed: {out:?}");
    let screen = String::from_utf8_lossy(&out.stdout);
    assert!(screen.contains("sendmark-42"), "peek screen: {screen}");
}

/// `wait --idle` returns once output settles; a condition that never holds
/// times out with the documented exit code 4.
#[tokio::test]
async fn wait_idle_and_timeout() {
    let daemon = Daemon::start("waitidle");
    assert!(
        daemon
            .cli()
            .args(["new", "quiet"])
            .output()
            .unwrap()
            .status
            .success()
    );

    // A fresh shell prints its prompt then goes quiet: --idle fires within the
    // 2s settle window.
    let out = daemon
        .cli()
        .args(["wait", "quiet", "--idle", "--timeout", "10s"])
        .output()
        .unwrap();
    assert!(out.status.success(), "wait --idle failed: {out:?}");

    // A never-satisfied condition times out → exit 4 (boo's code).
    let out = daemon
        .cli()
        .args([
            "wait",
            "quiet",
            "--text",
            "never-appears",
            "--timeout",
            "500ms",
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(4),
        "expected timeout exit 4: {out:?}"
    );
}

/// `peek --json` emits geometry + screen as one JSON object; `peek`/`send` on a
/// missing session fail.
#[tokio::test]
async fn peek_json_and_missing_session() {
    let daemon = Daemon::start("peekjson");
    assert!(
        daemon
            .cli()
            .args(["new", "js"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let out = daemon
        .cli()
        .args(["peek", "js", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "peek --json failed: {out:?}");
    let json = String::from_utf8_lossy(&out.stdout);
    // Default create size is 80x24, and peek does not attach/resize.
    assert!(json.contains("\"session\":\"js\""), "json: {json}");
    assert!(json.contains("\"rows\":24"), "json: {json}");
    assert!(json.contains("\"cols\":80"), "json: {json}");
    assert!(json.contains("\"screen\":"), "json: {json}");

    // Missing session → non-zero exit for both scripting commands.
    assert!(
        !daemon
            .cli()
            .args(["peek", "nope"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !daemon
            .cli()
            .args(["send", "nope", "--text", "x"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

/// v5: `SessionInfo.running` tracks output activity — true while the session is
/// producing output, false once it has been idle past `IDLE_SETTLE_MS`.
#[tokio::test]
async fn running_flag_tracks_activity() {
    let daemon = Daemon::start("running");
    assert!(
        daemon
            .cli()
            .args(["new", "act"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let mut c = ProtoClient::connect(&daemon.socket).await;

    // Trigger a fresh burst of output without attaching (v4 SendInput).
    c.send(Frame::SendInput {
        name: "act".into(),
        bytes: b"printf act-running\n".to_vec(),
    })
    .await;
    match c.recv().await {
        Frame::Ack => {}
        other => panic!("expected Ack, got {other:?}"),
    }

    // running is true while that output is fresh (idle_ms < IDLE_SETTLE_MS).
    let deadline = tokio::time::Instant::now() + WAIT;
    let saw_running = loop {
        c.send(Frame::ListSessions).await;
        if list_find(&mut c, "act").await.running {
            break true;
        }
        if tokio::time::Instant::now() >= deadline {
            break false;
        }
        tokio::time::sleep(TICK).await;
    };
    assert!(saw_running, "session never reported running after a burst");

    // After the settle window with no further output, running clears.
    tokio::time::sleep(Duration::from_millis(asd_proto::IDLE_SETTLE_MS + 700)).await;
    c.send(Frame::ListSessions).await;
    let s = list_find(&mut c, "act").await;
    assert!(
        !s.running,
        "session still running after settling: idle_ms={}",
        s.idle_ms
    );
}

/// v6 `inspect` dumps one session's metadata + live terminal state, as a
/// labeled block or JSON; a missing session fails.
#[tokio::test]
async fn inspect_dumps_session_detail() {
    let daemon = Daemon::start("inspect");
    assert!(
        daemon
            .cli()
            .args(["new", "insp"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let out = daemon.cli().args(["inspect", "insp"]).output().unwrap();
    assert!(out.status.success(), "inspect failed: {out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    // Default create size, primary screen (a plain shell), and the labeled
    // internals are all present.
    assert!(text.contains("insp"), "text: {text}");
    assert!(text.contains("80x24"), "text: {text}");
    assert!(text.contains("primary"), "text: {text}");
    assert!(text.contains("scrollback"), "text: {text}");
    assert!(text.contains("cursor"), "text: {text}");

    let out = daemon
        .cli()
        .args(["inspect", "insp", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "inspect --json failed: {out:?}");
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(json.contains("\"session\":\"insp\""), "json: {json}");
    assert!(json.contains("\"cols\":80"), "json: {json}");
    assert!(json.contains("\"alt_screen\":false"), "json: {json}");
    assert!(json.contains("\"pid\":"), "json: {json}");
    assert!(json.contains("\"cursor\":{\"col\":"), "json: {json}");
    assert!(
        !json.contains("\"pid\":0"),
        "child pid should be live: {json}"
    );

    // Missing session → non-zero exit.
    assert!(
        !daemon
            .cli()
            .args(["inspect", "nope"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

/// Find a named session in the next `SessionList` reply.
async fn list_find(c: &mut ProtoClient, name: &str) -> asd_proto::SessionInfo {
    match c.recv_skipping_output().await {
        Frame::SessionList { sessions } => sessions
            .into_iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("session {name} not listed")),
        other => panic!("expected SessionList, got {other:?}"),
    }
}
