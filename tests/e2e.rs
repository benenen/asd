//! Integration tests (spec §8): real UDS + real daemon process + the `asd` CLI.
//!
//! Coverage: create → write → attach asserting the snapshot → detach →
//! re-attach asserting the accumulated output; multi-client broadcast;
//! version-mismatch rejection; --stdio proxy passthrough; no leftover child
//! processes and socket cleanup after daemon SIGTERM.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use asd_proto::{
    ClientKind, Frame, FrameReader, FrameWriter, PROTO_VERSION, TerminalAppearance, TerminalColor,
    code,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::time::timeout;

const TICK: Duration = Duration::from_millis(50);
const WAIT: Duration = Duration::from_secs(10);
static PTSNAME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        // Match Daemon::start's data dir so CLI subcommands that spawn a daemon
        // (e.g. `asd restart` re-exec'ing a successor) use the test's isolated
        // sessions.tsv, mirroring production where the daemon and CLI share the
        // shell's XDG_DATA_HOME.
        cmd.env("XDG_DATA_HOME", self.dir.join("data"));
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

    /// Start a fresh daemon on the same socket + data dir (as a detached process,
    /// not our child) and wait for it to accept connections. Used to test restore
    /// after the original daemon has stopped. Returns the child so the caller can
    /// SIGTERM it at the end.
    fn respawn_successor(&self) -> std::process::Child {
        let child = Command::new(cli_exe())
            .arg("daemon")
            .arg("--socket")
            .arg(&self.socket)
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + WAIT;
        while !self.socket.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "successor daemon never came up"
            );
            std::thread::sleep(TICK);
        }
        child
    }

    /// SIGTERM this daemon and wait for its socket to disappear.
    fn stop_and_wait(&self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + WAIT;
        while self.socket.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "daemon didn't shut down after SIGTERM"
            );
            std::thread::sleep(TICK);
        }
    }

    /// The real working directory of a session's child process: `asd inspect
    /// --json` reports the child pid, then `/proc/<pid>/cwd` is its (canonical)
    /// cwd. `None` if the session/pid/proc entry isn't available. This is a
    /// deterministic cwd check — unlike scraping `pwd` off the rendered screen,
    /// which races the shell's readiness and can match a marker echoed in the
    /// command line before the command runs.
    fn session_cwd(&self, name: &str) -> Option<PathBuf> {
        let out = self.cli().args(["inspect", name, "--json"]).output().ok()?;
        let json = String::from_utf8_lossy(&out.stdout);
        let pid: u32 = json
            .split("\"pid\":")
            .nth(1)?
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()?;
        if pid == 0 {
            return None;
        }
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// Poll until `name`'s child cwd equals `want`, or panic after `WAIT`.
    fn wait_session_cwd(&self, name: &str, want: &Path) {
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            let got = self.session_cwd(name);
            if got.as_deref() == Some(want) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{name} cwd never became {want:?} (got {got:?})"
            );
            std::thread::sleep(TICK);
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
    kind: ClientKind,
    next_view_id: u64,
}

impl ProtoClient {
    async fn connect(socket: &Path) -> Self {
        Self::connect_kind(socket, ClientKind::Cli).await
    }

    async fn connect_kind(socket: &Path, kind: ClientKind) -> Self {
        let stream = UnixStream::connect(socket).await.expect("connect failed");
        let (r, w) = stream.into_split();
        let mut c = Self {
            reader: FrameReader::new(r),
            writer: FrameWriter::new(w),
            kind,
            next_view_id: 1,
        };
        c.send(Frame::Hello {
            proto_version: PROTO_VERSION,
            kind,
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
        self.attach_sized(name, 80, 24).await
    }

    /// Attach as a client of a given window size.
    async fn attach_sized(&mut self, name: &str, cols: u16, rows: u16) -> Vec<u8> {
        let view_id = if self.kind == ClientKind::Tui {
            let view_id = self.next_view_id;
            self.next_view_id += 1;
            view_id
        } else {
            0
        };
        self.send(Frame::Attach {
            name: name.into(),
            cols,
            rows,
            view_id,
            appearance: asd_proto::TerminalAppearance::default(),
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

/// A program can query its terminal theme before anyone attaches. The daemon
/// must hold that query rather than invent black, then answer it from the first
/// real terminal appearance carried by Attach.
#[tokio::test]
async fn appearance_answers_query_that_predates_attach() {
    let daemon = Daemon::start("appearance");
    let command = "stty raw -echo; printf '\\033]11;?\\007QUERY_READY\\n'; \
                   dd bs=1 count=24 2>/dev/null | od -An -tx1 | tr -d ' \\n'; \
                   printf '\\n'; sleep 5";
    let out = daemon
        .cli()
        .args(["new", "colors", "--cmd", command])
        .output()
        .unwrap();
    assert!(out.status.success(), "create failed: {out:?}");

    let waited = daemon
        .cli()
        .args(["wait", "colors", "--text", "QUERY_READY", "--timeout", "5s"])
        .output()
        .unwrap();
    assert!(
        waited.status.success(),
        "query was not pending before attach: {}",
        String::from_utf8_lossy(&waited.stderr)
    );

    let mut client = ProtoClient::connect(&daemon.socket).await;
    client
        .send(Frame::Attach {
            name: "colors".into(),
            cols: 80,
            rows: 24,
            view_id: 0,
            appearance: TerminalAppearance {
                foreground: None,
                background: Some(TerminalColor {
                    r: 0x0a,
                    g: 0x14,
                    b: 0x1e,
                }),
            },
        })
        .await;
    match client.recv().await {
        Frame::Snapshot { .. } => {}
        other => panic!("expected Snapshot, got {other:?}"),
    }

    client
        .read_output_until(b"1b5d31313b7267623a306130612f313431342f3165316507")
        .await;
}

/// Once attached, clients must not see an OSC color query: GUI terminal
/// mirrors would answer it too. The daemon keeps the query for its own VT and
/// the child receives exactly one response.
#[tokio::test]
async fn attached_color_query_is_daemon_only_and_answered_once() {
    let daemon = Daemon::start("attached-appearance");
    let command = "stty raw -echo; printf 'ATTACHED_READY\\n'; \
                   dd bs=1 count=1 >/dev/null 2>&1; \
                   printf '\\033]11;?\\007\\23511;?\\234REPLY:'; \
                   dd bs=1 count=48 2>/dev/null | od -An -tx1 | tr -d ' \\n'; \
                   printf '\\nEXTRA:'; \
                   timeout 1 dd bs=1 count=48 2>/dev/null | od -An -tx1 | tr -d ' \\n'; \
                   printf '\\n'; sleep 5";
    let out = daemon
        .cli()
        .args(["new", "attached-colors", "--cmd", command])
        .output()
        .unwrap();
    assert!(out.status.success(), "create failed: {out:?}");
    let waited = daemon
        .cli()
        .args([
            "wait",
            "attached-colors",
            "--text",
            "ATTACHED_READY",
            "--timeout",
            "5s",
        ])
        .output()
        .unwrap();
    assert!(
        waited.status.success(),
        "child was not ready before attach: {}",
        String::from_utf8_lossy(&waited.stderr)
    );

    let mut client = ProtoClient::connect(&daemon.socket).await;
    client
        .send(Frame::Attach {
            name: "attached-colors".into(),
            cols: 80,
            rows: 24,
            view_id: 0,
            appearance: TerminalAppearance {
                foreground: None,
                background: Some(TerminalColor {
                    r: 0x0a,
                    g: 0x14,
                    b: 0x1e,
                }),
            },
        })
        .await;
    match client.recv().await {
        Frame::Snapshot { .. } => {}
        other => panic!("expected Snapshot, got {other:?}"),
    }
    client.send(Frame::Input { bytes: vec![b'g'] }).await;

    let expected = b"REPLY:1b5d31313b7267623a306130612f313431342f3165316507\
                     1b5d31313b7267623a306130612f313431342f316531659c\nEXTRA:\n";
    let mut observed = Vec::new();
    while !observed
        .windows(expected.len())
        .any(|window| window == expected)
    {
        match client.recv().await {
            Frame::Output { bytes } => observed.extend_from_slice(&bytes),
            Frame::Error { code, msg } => {
                panic!("daemon error {code}: {msg}; output={observed:?}")
            }
            _ => {}
        }
    }

    assert!(
        !observed
            .windows(b"\x1b]11;?\x07".len())
            .any(|window| window == b"\x1b]11;?\x07"),
        "client saw the daemon-only 7-bit query: {observed:?}"
    );
    assert!(
        !observed
            .windows(b"\x9d11;?\x9c".len())
            .any(|window| window == b"\x9d11;?\x9c"),
        "client saw the daemon-only C1 query: {observed:?}"
    );
    for reply_hex in [
        b"1b5d31313b7267623a306130612f313431342f3165316507".as_slice(),
        b"1b5d31313b7267623a306130612f313431342f316531659c".as_slice(),
    ] {
        assert_eq!(
            observed
                .windows(reply_hex.len())
                .filter(|window| *window == reply_hex)
                .count(),
            1,
            "child did not receive exactly one reply: {observed:?}"
        );
    }
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

/// `asd rename` changes a session's name **without touching what is running in
/// it** — that is the whole point: a session created with an auto-generated or
/// prefixed name can be corrected instead of killed and recreated.
///
/// The daemon has handled `Frame::Rename` since v7 and the TUI has exposed it as
/// `r` all along; this covers the scripting entry point added on top.
#[tokio::test]
async fn rename_via_cli_keeps_the_running_program() {
    let daemon = Daemon::start("rename");

    // Print a marker, then stay alive: the marker proves the pty survived.
    let out = daemon
        .cli()
        .args([
            "new",
            "old-name",
            "--cmd",
            "sh -c 'echo RENAME-MARKER; sleep 300'",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    // Wait for the marker so we know the program actually ran before the rename.
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let out = daemon.cli().args(["peek", "old-name"]).output().unwrap();
        if String::from_utf8_lossy(&out.stdout).contains("RENAME-MARKER") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "marker never appeared"
        );
        std::thread::sleep(TICK);
    }

    let out = daemon
        .cli()
        .args(["rename", "old-name", "new-name"])
        .output()
        .unwrap();
    assert!(out.status.success(), "rename failed: {out:?}");
    // Echoes the settled name, the way `new` does, so scripts can read it back.
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "new-name");

    let out = daemon.cli().arg("list").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("new-name"), "list output: {stdout}");
    assert!(!stdout.contains("old-name"), "old name lingers: {stdout}");

    // The running program and its screen are untouched — this is the property
    // that makes rename worth having over kill-and-recreate.
    let out = daemon.cli().args(["peek", "new-name"]).output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("RENAME-MARKER"),
        "screen lost across rename: {out:?}"
    );

    // A missing session uses the same exit code as peek/send/kill (3), so
    // scripting wrappers can keep one mapping for "no such session".
    let out = daemon
        .cli()
        .args(["rename", "ghost", "whatever"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Colliding with an existing name is a plain failure, not "no such session".
    daemon
        .cli()
        .args(["new", "taken", "--cmd", "sleep 300"])
        .output()
        .unwrap();
    let out = daemon
        .cli()
        .args(["rename", "new-name", "taken"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_ne!(out.status.code(), Some(3));
}

/// `asd restart` stops the running daemon (by signal, via the pid file) and
/// brings up a fresh one; sessions are dropped. This is the recovery path for a
/// protocol-version bump, where the client can't handshake the old daemon.
#[tokio::test]
async fn restart_replaces_the_daemon() {
    let mut daemon = Daemon::start("restart");
    let old_pid = daemon.child.id();

    // A session that should survive the restart (its workspace is restored).
    assert!(
        daemon
            .cli()
            .args(["new", "kept"])
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

    // A fresh daemon is up under a new pid, answers `list`, and the session was
    // recreated (its workspace is restored across the restart).
    let new_pid: i32 = std::fs::read_to_string(daemon.socket.with_extension("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_ne!(new_pid as u32, old_pid, "restart reused the old pid");
    let list = daemon.cli().arg("list").output().unwrap();
    assert!(list.status.success(), "list after restart failed: {list:?}");
    assert!(
        String::from_utf8_lossy(&list.stdout).contains("kept"),
        "session should survive restart (workspace restored): {}",
        String::from_utf8_lossy(&list.stdout)
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

/// `send --enter` is one session-thread operation: concurrent callers may be
/// ordered either way, but one caller's text cannot land between the other's
/// text and Enter.
#[tokio::test]
async fn concurrent_send_enter_sequences_do_not_interleave() {
    let daemon = Daemon::start("sendatomic");
    assert!(
        daemon
            .cli()
            .args(["new", "work"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let spawn_send = |marker: &str, value: &str| {
        let mut command = daemon.cli();
        command
            .args([
                "send",
                "work",
                "--text",
                &format!("printf '{marker}-%s\\n' {value}"),
                "--enter",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.spawn().unwrap()
    };
    let first = spawn_send("atomic-A", "17");
    let second = spawn_send("atomic-B", "23");
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    assert!(first.status.success(), "first send failed: {first:?}");
    assert!(second.status.success(), "second send failed: {second:?}");

    let deadline = std::time::Instant::now() + WAIT;
    let screen = loop {
        let output = daemon.cli().args(["peek", "work"]).output().unwrap();
        assert!(output.status.success(), "peek failed: {output:?}");
        let screen = String::from_utf8_lossy(&output.stdout).into_owned();
        if screen.contains("atomic-A-17") && screen.contains("atomic-B-23") {
            break screen;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "concurrent commands did not both execute: {screen}"
        );
        std::thread::sleep(TICK);
    };
    assert!(screen.contains("atomic-A-17"));
    assert!(screen.contains("atomic-B-23"));
}

/// `asd follow` streams a session's output as it is produced and returns on its
/// own once the session settles — the daemon's idle signal, the same one
/// `wait --idle` uses, delivered inline with the stream instead of polled.
#[tokio::test]
async fn follow_until_idle() {
    let daemon = Daemon::start("follow");
    assert!(
        daemon
            .cli()
            .args(["new", "fol"])
            .output()
            .unwrap()
            .status
            .success()
    );

    // The pty echoes the command line, so the markers must not appear in the
    // command *text* — `LINE%s` is typed, `LINE1` only exists once printf has
    // run. Otherwise the echo alone would satisfy the assertions and the test
    // would pass without anything having been streamed.
    //
    // The leading sleep is the handshake window: `send` returns as soon as the
    // daemon acks it, so `follow` has to get subscribed before the first line
    // is printed.
    let out = daemon
        .cli()
        .args([
            "send",
            "fol",
            "--text",
            "sleep 1; for i in 1 2 3; do printf 'LINE%s\\n' \"$i\"; sleep 0.2; done",
            "--enter",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "send failed: {out:?}");

    // Blocks until the session goes idle, then exits 0 on its own.
    let out = daemon
        .cli()
        .args(["follow", "fol", "--timeout", "20s"])
        .output()
        .unwrap();
    assert!(out.status.success(), "follow failed: {out:?}");

    let streamed = String::from_utf8_lossy(&out.stdout);
    for marker in ["LINE1", "LINE2", "LINE3"] {
        assert!(
            streamed.contains(marker),
            "{marker} missing from: {streamed}"
        );
    }
}

/// `follow --forever` keeps streaming across a quiet spell that would have
/// ended the default follow, and still returns when the session itself ends.
///
/// The second half matters as much as the first: a follower is not in the
/// session's client list, so it never sees the `SESSION_EXITED` broadcast, and
/// `--forever` is precisely the mode that ignores the idle status. Without an
/// end-of-session signal of its own it would sit there until its timeout for a
/// session that is already gone.
#[tokio::test]
async fn follow_forever_streams_past_a_quiet_spell() {
    let daemon = Daemon::start("followfvr");
    assert!(
        daemon
            .cli()
            .args(["new", "fvr"])
            .output()
            .unwrap()
            .status
            .success()
    );

    // The gap is longer than IDLE_SETTLE_MS (2s), so a default `follow` would
    // have returned after FIRST. The trailing `exit` ends the session, which is
    // what this mode stops on. As above, the markers exist only in the output:
    // `%s` is what gets echoed.
    let out = daemon
        .cli()
        .args([
            "send",
            "fvr",
            "--text",
            "sleep 1; printf 'FIRST%s\\n' ''; sleep 3; printf 'SECOND%s\\n' ''; exit",
            "--enter",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "send failed: {out:?}");

    let out = daemon
        .cli()
        .args(["follow", "fvr", "--forever", "--timeout", "20s"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "follow --forever did not end with the session: {out:?}"
    );

    let streamed = String::from_utf8_lossy(&out.stdout);
    for marker in ["FIRST", "SECOND"] {
        assert!(
            streamed.contains(marker),
            "{marker} missing from: {streamed}"
        );
    }
}

/// `follow --json` is the same stream as JSONL, but modelled rather than
/// echoed: `output` carries the lines that scrolled off the session's screen —
/// final, in order, once — and `screen` carries the live screen at each pause.
/// The payload is raw pty bytes, so an unescaped or half-decoded chunk would
/// break the format for every consumer downstream.
#[tokio::test]
async fn follow_json_emits_one_event_per_line() {
    let daemon = Daemon::start("followjson");
    assert!(
        daemon
            .cli()
            .args(["new", "folj"])
            .output()
            .unwrap()
            .status
            .success()
    );

    // The marker must exist only in the *output*, never in the echoed command
    // line — `%s` is what gets typed. 30 lines overflow the 24-row default
    // screen, so the early ones scroll off and become `output`; the rest are
    // still live and arrive as `screen`. The trailing `exit` ends the session.
    let out = daemon
        .cli()
        .args([
            "send",
            "folj",
            "--text",
            "sleep 1; for i in $(seq 1 30); do printf 'JMARK%s \\342\\234\\263\\n' \"$i\"; done; exit",
            "--enter",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "send failed: {out:?}");

    let out = daemon
        .cli()
        .args(["follow", "folj", "--forever", "--json", "--timeout", "20s"])
        .output()
        .unwrap();
    assert!(out.status.success(), "follow --json failed: {out:?}");

    let streamed = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = streamed.lines().filter(|l| !l.is_empty()).collect();
    assert!(!lines.is_empty(), "no events: {streamed}");
    for line in &lines {
        assert!(
            line.starts_with(r#"{"event":""#) && line.ends_with('}'),
            "not one object per line: {line}"
        );
        assert!(
            line.contains(r#""time_ms":"#),
            "event without a time: {line}"
        );
    }
    // Lines that scrolled away are final, and reported as such.
    assert!(
        lines
            .iter()
            .any(|l| l.contains(r#""event":"output""#) && l.contains("JMARK1")),
        "scrolled-off line missing from: {streamed}"
    );
    // What never scrolled is still reported — as the screen, at the pause.
    assert!(
        lines.iter().any(|l| l.contains(r#""event":"screen""#)),
        "no screen event in: {streamed}"
    );
    assert!(
        lines.iter().any(|l| l.contains("JMARK30")),
        "last line missing from: {streamed}"
    );
    // Non-ASCII survives the terminal round-trip intact.
    assert!(
        lines.iter().any(|l| l.contains('✳')),
        "multi-byte character mangled: {streamed}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains(r#""event":"status""#) && l.contains(r#""running":false"#)),
        "no settle status in: {streamed}"
    );
    assert!(
        lines.last().unwrap().starts_with(r#"{"event":"exit""#),
        "stream did not end with the session: {streamed}"
    );
    // Modelled, so escape sequences cannot appear at all: text comes from the
    // terminal's cells, not from the wire.
    assert!(
        !streamed.contains("\\u001b") && !streamed.contains('\u{1b}'),
        "escape sequences in modelled output: {streamed}"
    );

    // --raw is the opt-out: the verbatim stream, escapes intact.
    assert!(
        daemon
            .cli()
            .args(["new", "folr"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let out = daemon
        .cli()
        .args([
            "send",
            "folr",
            "--text",
            "sleep 1; printf '\\033[31mRMARK%s\\033[0m\\n' ''; exit",
            "--enter",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "send failed: {out:?}");
    let out = daemon
        .cli()
        .args([
            "follow",
            "folr",
            "--forever",
            "--json",
            "--raw",
            "--timeout",
            "20s",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "follow --json --raw failed: {out:?}");
    let raw = String::from_utf8_lossy(&out.stdout);
    assert!(
        raw.contains("\\u001b[31mRMARK"),
        "--raw dropped the escapes: {raw}"
    );
    // Still JSONL, and still no unescaped control bytes in it.
    assert!(!raw.contains('\u{1b}'), "raw ESC byte in JSONL: {raw}");
}

/// The reason `follow --json` models a terminal instead of stripping escapes:
/// a program that rewrites one line in place — every TUI status line, from
/// Claude Code's spinner to a progress bar — emits a frame's worth of bytes
/// several times a second, and none of it is new output. Stripping escapes
/// cannot tell those from content, because the escapes *are* the distinction;
/// a terminal can, because a row that has not left the screen can still change.
#[tokio::test]
async fn follow_json_does_not_log_a_repainted_status_line() {
    let daemon = Daemon::start("followrepaint");
    assert!(
        daemon
            .cli()
            .args(["new", "folp"])
            .output()
            .unwrap()
            .status
            .success()
    );

    // 10 frames of a spinner: CR, erase-line, new glyph and counter. Nothing
    // ever scrolls, so nothing is ever final.
    let out = daemon
        .cli()
        .args([
            "send",
            "folp",
            "--text",
            "sleep 1; for i in $(seq 1 10); do printf '\\r\\033[2K* SPIN%s %s' '' \"$i\"; sleep 0.15; done; exit",
            "--enter",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "send failed: {out:?}");

    let out = daemon
        .cli()
        .args(["follow", "folp", "--forever", "--json", "--timeout", "20s"])
        .output()
        .unwrap();
    assert!(out.status.success(), "follow --json failed: {out:?}");

    let streamed = String::from_utf8_lossy(&out.stdout);
    let frames = streamed.matches("SPIN").count();
    assert!(
        frames <= 2,
        "the repaint was logged frame by frame ({frames} times): {streamed}"
    );
    // It is reported — once, as the screen, holding the last frame.
    assert!(
        streamed.contains(r#""event":"screen""#) && streamed.contains("SPIN 10"),
        "the repainted line was never reported: {streamed}"
    );

    // --raw sees every frame: that is what the model is filtering.
    assert!(
        daemon
            .cli()
            .args(["new", "folp2"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let out = daemon
        .cli()
        .args([
            "send",
            "folp2",
            "--text",
            "sleep 1; for i in $(seq 1 10); do printf '\\r\\033[2K* SPIN%s %s' '' \"$i\"; sleep 0.15; done; exit",
            "--enter",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "send failed: {out:?}");
    let out = daemon
        .cli()
        .args([
            "follow",
            "folp2",
            "--forever",
            "--json",
            "--raw",
            "--timeout",
            "20s",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "follow --raw failed: {out:?}");
    let raw = String::from_utf8_lossy(&out.stdout);
    assert!(
        raw.matches("SPIN").count() > 5,
        "--raw should have every frame: {raw}"
    );
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

/// `peek --scrollback` takes an optional line count, and the three states are
/// three different requests: absent is the screen alone, bare is the whole
/// history, and a value keeps the last N lines above the screen.
///
/// The count is applied by the daemon rather than the caller: a session can
/// retain tens of thousands of lines, and the reply has to fit in one frame, so
/// "the last 10 lines" must not mean "send everything and let the client cut".
#[tokio::test]
async fn peek_scrollback_takes_an_optional_limit() {
    let daemon = Daemon::start("peeksb");
    assert!(
        daemon
            .cli()
            .args(["new", "sb"])
            .output()
            .unwrap()
            .status
            .success()
    );

    // 200 numbered lines: far more than the 24-row screen, so most of them are
    // history. As elsewhere, the marker exists only in the output — `%s` is
    // what the echoed command line shows.
    let out = daemon
        .cli()
        .args([
            "send",
            "sb",
            "--text",
            "for i in $(seq 1 200); do printf 'HIST%s\\n' \"$i\"; done",
            "--enter",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "send failed: {out:?}");
    assert!(
        daemon
            .cli()
            .args(["wait", "sb", "--idle", "--timeout", "20s"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let peek = |args: &[&str]| -> Vec<String> {
        let out = daemon.cli().args(args).output().unwrap();
        assert!(out.status.success(), "peek failed: {out:?}");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    };

    // No flag: the screen, and nothing above it.
    let screen = peek(&["peek", "sb"]);
    assert_eq!(screen.len(), 24, "screen: {screen:?}");
    assert!(
        !screen.iter().any(|l| l.contains("HIST100")),
        "history leaked into a plain peek: {screen:?}"
    );

    // Bare: everything the session still holds.
    let all = peek(&["peek", "sb", "--scrollback"]);
    assert!(
        all.iter().any(|l| l.contains("HIST1"))
            && all.iter().any(|l| l.contains("HIST200"))
            && all.len() > 200,
        "full history missing lines: {} lines",
        all.len()
    );

    // Valued: the screen plus exactly that many lines above it.
    let limited = peek(&["peek", "sb", "--scrollback", "10"]);
    assert_eq!(limited.len(), 34, "10 + 24 rows expected: {limited:?}");
    assert!(
        limited.iter().any(|l| l.contains("HIST200")),
        "the screen is always included: {limited:?}"
    );
    assert!(
        !limited.iter().any(|l| l.contains("HIST100")),
        "the limit was not applied: {limited:?}"
    );

    // Degenerate values behave: none, and more than exists.
    assert_eq!(peek(&["peek", "sb", "--scrollback", "0"]).len(), 24);
    assert_eq!(
        peek(&["peek", "sb", "--scrollback", "99999"]).len(),
        all.len()
    );
}

/// `asd card` answers "what is this session for" — the project documents in its
/// working directory — so an agent can pick a session before running anything
/// in it. Three levels: `list` (where each session is), `inspect` (headings and
/// excerpts), `cat` (one file in full).
///
/// The directory comes from the session's own process, so this only works
/// against a local daemon; the e2e daemon is a child process here, which is
/// exactly that case.
#[tokio::test]
async fn card_reports_the_documents_in_a_session_directory() {
    let daemon = Daemon::start("card");
    let proj = daemon.dir.join("proj");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    std::fs::write(
        proj.join("README.md"),
        "# widget-api\n\nA REST service for widget inventory.\n\n## Running\n\n`make dev`\n",
    )
    .unwrap();
    std::fs::write(
        proj.join("AGENTS.md"),
        "# agents\n\nRun `make test` before every commit.\n",
    )
    .unwrap();
    std::fs::write(proj.join("src/main.rs"), "fn main() {}\n").unwrap();

    assert!(
        daemon
            .cli()
            .args(["new", "cardsess", "--cwd"])
            .arg(&proj)
            .output()
            .unwrap()
            .status
            .success()
    );
    // The card reads the *live* cwd, so wait until the shell is actually there.
    daemon.wait_session_cwd("cardsess", &proj.canonicalize().unwrap());

    // list: one row per session, with where it is and what it holds.
    let out = daemon
        .cli()
        .args(["card", "list", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "card list failed: {out:?}");
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(json.contains(r#""session":"cardsess""#), "json: {json}");
    assert!(
        json.contains(r#""docs":["README.md","AGENTS.md"]"#),
        "documents not reported in order: {json}"
    );
    // Bare `asd card` is the same listing, in table form.
    let out = daemon.cli().args(["card"]).output().unwrap();
    let table = String::from_utf8_lossy(&out.stdout);
    assert!(
        table.contains("NAME") && table.contains("cardsess") && table.contains("README.md"),
        "bare card is not the listing: {table}"
    );

    // inspect: what each document says, without fetching them.
    let out = daemon
        .cli()
        .args(["card", "inspect", "cardsess", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "card inspect failed: {out:?}");
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(json.contains(r#""heading":"widget-api""#), "json: {json}");
    assert!(
        json.contains("A REST service for widget inventory."),
        "excerpt missing: {json}"
    );
    // The `## Running` heading is dropped from the excerpt — a card carries
    // prose, not a table of contents.
    assert!(!json.contains("## Running"), "heading in excerpt: {json}");

    // cat: any file under the directory, not just the documents.
    let out = daemon
        .cli()
        .args(["card", "cat", "cardsess", "src/main.rs"])
        .output()
        .unwrap();
    assert!(out.status.success(), "card cat failed: {out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "fn main() {}\n");

    // …and nothing outside it: traversal and absolute paths are refused.
    for bad in ["../../../etc/passwd", "/etc/passwd"] {
        let out = daemon
            .cli()
            .args(["card", "cat", "cardsess", bad])
            .output()
            .unwrap();
        assert!(!out.status.success(), "card cat allowed {bad}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("outside the session's directory"),
            "unexpected error for {bad}: {out:?}"
        );
    }

    // A missing session reports it the way every other command does.
    let out = daemon
        .cli()
        .args(["card", "inspect", "ghost"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "missing session: {out:?}");
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
        enter: false,
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

/// v7 rename: a `Rename` moves the session's name across list + attach;
/// invalid / duplicate / missing names are rejected with the right codes.
#[tokio::test]
async fn rename_session() {
    let daemon = Daemon::start("rename");
    for n in ["old", "other"] {
        assert!(
            daemon
                .cli()
                .args(["new", n])
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let mut c = ProtoClient::connect(&daemon.socket).await;

    // old → newname, acked.
    c.send(Frame::Rename {
        name: "old".into(),
        new_name: "newname".into(),
    })
    .await;
    match c.recv_skipping_output().await {
        Frame::Ack => {}
        other => panic!("expected Ack, got {other:?}"),
    }

    // The list shows the new name and not the old.
    c.send(Frame::ListSessions).await;
    match c.recv_skipping_output().await {
        Frame::SessionList { sessions } => {
            assert!(
                sessions.iter().any(|s| s.name == "newname"),
                "new name present"
            );
            assert!(!sessions.iter().any(|s| s.name == "old"), "old name gone");
        }
        other => panic!("expected SessionList, got {other:?}"),
    }

    // Rejections: duplicate target, invalid chars, missing source.
    for (name, new_name, want) in [
        ("newname", "other", code::SESSION_EXISTS),
        ("newname", "bad name", code::INVALID_NAME),
        ("ghost", "whatever", code::NO_SUCH_SESSION),
    ] {
        c.send(Frame::Rename {
            name: name.into(),
            new_name: new_name.into(),
        })
        .await;
        match c.recv_skipping_output().await {
            Frame::Error { code, .. } => assert_eq!(code, want, "{name}->{new_name}"),
            other => panic!("expected Error {want}, got {other:?}"),
        }
    }

    // Attach by the new name works — the map key really moved.
    let snap = c.attach("newname").await;
    assert!(!snap.is_empty());
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

/// A session that dies while a connection is attached must not wedge that
/// connection: re-attaching to a different session on the same connection has
/// to succeed (a fresh Snapshot), never be rejected as "already attached".
///
/// Regression for the asd-tui "blank pane after kill-then-new-session" bug. The
/// daemon leaked the per-connection `attached` state when the session died
/// under it (the session thread can't reach the connection's read-side
/// bookkeeping), so the *next* Attach on that connection saw a stale attachment
/// and replied `ALREADY_ATTACHED` with no Snapshot — leaving the client's pane
/// permanently blank until it reconnected. Attaching now supersedes any prior
/// attachment.
#[tokio::test]
async fn attach_after_attached_session_dies_is_not_wedged() {
    let daemon = Daemon::start("reattach");

    // Session A, created and attached.
    assert!(
        daemon.cli().args(["new", "a"]).status().unwrap().success(),
        "create a failed"
    );
    let mut c = ProtoClient::connect(&daemon.socket).await;
    let _ = c.attach("a").await;

    // Kill A out from under the attached connection (a separate CLI client).
    assert!(
        daemon.cli().args(["kill", "a"]).status().unwrap().success(),
        "kill a failed"
    );

    // The attached connection is told the session exited (drain any trailing
    // Output first). Like asd-tui, we treat this as "detached" and do NOT send
    // a Detach — exactly the path that used to wedge the connection.
    loop {
        match c.recv().await {
            Frame::Output { .. } => continue,
            Frame::Error { code, .. } => {
                assert_eq!(code, code::SESSION_EXITED, "expected SESSION_EXITED");
                break;
            }
            other => panic!("expected SESSION_EXITED, got {other:?}"),
        }
    }

    // Session B, then re-attach on the SAME connection without a Detach.
    assert!(
        daemon.cli().args(["new", "b"]).status().unwrap().success(),
        "create b failed"
    );
    c.send(Frame::Attach {
        name: "b".into(),
        cols: 80,
        rows: 24,
        view_id: 0,
        appearance: asd_proto::TerminalAppearance::default(),
    })
    .await;
    match c.recv_skipping_output().await {
        Frame::Snapshot { .. } => {} // re-attach succeeded
        Frame::Error { code, msg } => panic!(
            "re-attach rejected (code {code}): {msg} — connection wedged by the dead session"
        ),
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

/// `asd restart` (SIGUSR1) records each session's working directory; the
/// successor daemon recreates the session as a fresh shell in that directory.
/// Regression for "restart preserves each session's workspace".
#[tokio::test]
async fn restart_preserves_session_workspace() {
    let daemon = Daemon::start("restartws");

    // A session, cd'd into a known directory.
    assert!(
        daemon
            .cli()
            .args(["new", "work"])
            .status()
            .unwrap()
            .success(),
        "create failed"
    );
    let workdir = daemon.dir.join("the-workspace");
    std::fs::create_dir_all(&workdir).unwrap();
    // The daemon captures the session cwd via /proc/<pid>/cwd, which resolves
    // symlinks. On hosts whose temp dir has a symlink component (e.g. CI
    // runners), the restored cwd is that physical path — so drive and assert
    // with the canonical path, not the logical (possibly-symlinked) one.
    let workdir = std::fs::canonicalize(&workdir).unwrap();
    daemon
        .cli()
        .args([
            "send",
            "work",
            "--text",
            &format!("cd '{}'", workdir.display()),
            "--enter",
        ])
        .status()
        .unwrap();
    // Wait until the cd has actually taken effect (the child's real cwd), so it
    // is captured before the daemon is asked to restart.
    daemon.wait_session_cwd("work", &workdir);

    // Restart: SIGUSR1 shuts the daemon down (the session list is already kept
    // persisted on disk continuously, so no special save-on-signal step needed).
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGUSR1) };
    let deadline = std::time::Instant::now() + WAIT;
    while daemon.socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "daemon didn't shut down after SIGUSR1"
        );
        std::thread::sleep(TICK);
    }

    // The persisted session list records name + cwd (in the daemon's data dir).
    let state = std::fs::read_to_string(daemon.dir.join("data/asd/sessions.tsv"))
        .expect("session list written");
    assert!(
        state.contains(&format!("work\t{}", workdir.display())),
        "state should record work's cwd, got: {state:?}"
    );

    // A fresh daemon on the same socket recreates the session in its cwd.
    let mut d2 = Command::new(cli_exe())
        .arg("daemon")
        .arg("--socket")
        .arg(&daemon.socket)
        .env("XDG_DATA_HOME", daemon.dir.join("data"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + WAIT;
    while !daemon.socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "successor daemon never came up"
        );
        std::thread::sleep(TICK);
    }

    // Session is back...
    let list = daemon.cli().args(["list"]).output().unwrap();
    let list = String::from_utf8_lossy(&list.stdout);
    assert!(list.contains("work"), "session not restored: {list}");

    // ...and its fresh shell is in the saved directory (the child's real cwd).
    // Poll without panicking so the detached successor `d2` is still reaped on
    // failure.
    let deadline = std::time::Instant::now() + WAIT;
    let mut in_cwd = false;
    while std::time::Instant::now() < deadline {
        if daemon.session_cwd("work").as_deref() == Some(workdir.as_path()) {
            in_cwd = true;
            break;
        }
        std::thread::sleep(TICK);
    }
    let _ = d2.kill();
    let _ = d2.wait();
    assert!(in_cwd, "restored shell is not in the saved cwd");
}

/// A plain daemon stop (SIGTERM, not `asd restart`) still persists the session
/// list, and a fresh daemon restores every session — cwd included.
#[tokio::test]
async fn sessions_persist_across_a_full_stop() {
    let daemon = Daemon::start("persist");
    for name in ["web", "logs"] {
        assert!(daemon.cli().args(["new", name]).status().unwrap().success());
    }
    let workdir = daemon.dir.join("web-workspace");
    std::fs::create_dir_all(&workdir).unwrap();
    // The daemon captures the session cwd via /proc/<pid>/cwd, which resolves
    // symlinks. On hosts whose temp dir has a symlink component (e.g. CI
    // runners), the restored cwd is that physical path — so drive and assert
    // with the canonical path, not the logical (possibly-symlinked) one.
    let workdir = std::fs::canonicalize(&workdir).unwrap();
    // cd web into workdir, then confirm the cd actually took effect by reading
    // the child's real cwd before stopping. (A screen marker like "READY" would
    // also match the echoed command line before `cd` even runs.)
    daemon
        .cli()
        .args([
            "send",
            "web",
            "--text",
            &format!("cd '{}'", workdir.display()),
            "--enter",
        ])
        .status()
        .unwrap();
    daemon.wait_session_cwd("web", &workdir);

    daemon.stop_and_wait();
    let mut successor = daemon.respawn_successor();

    let list = daemon.cli().args(["list"]).output().unwrap();
    let list = String::from_utf8_lossy(&list.stdout);
    assert!(
        list.contains("web") && list.contains("logs"),
        "both restored: {list}"
    );

    // The restored web session must be back in its saved cwd.
    daemon.wait_session_cwd("web", &workdir);

    unsafe { libc::kill(successor.id() as i32, libc::SIGTERM) };
    let _ = successor.wait();
}

/// Killing a session removes it from the persisted list, so a restart does not
/// bring it back — only the survivors return.
#[tokio::test]
async fn killed_session_is_not_restored() {
    let daemon = Daemon::start("nokill");
    for name in ["keep", "doomed"] {
        assert!(daemon.cli().args(["new", name]).status().unwrap().success());
    }
    assert!(
        daemon
            .cli()
            .args(["kill", "doomed"])
            .status()
            .unwrap()
            .success()
    );

    // `asd kill` is asynchronous (SIGHUP -> child EOF -> registry removal +
    // persist on the session thread). Wait until "doomed" has actually left the
    // live set before stopping, otherwise the shutdown freeze could snapshot it
    // while it's still live and resurrect it.
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let out = daemon.cli().args(["list"]).output().unwrap();
        if !String::from_utf8_lossy(&out.stdout).contains("doomed") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "doomed never left the session list after kill"
        );
        std::thread::sleep(TICK);
    }

    daemon.stop_and_wait();
    let mut successor = daemon.respawn_successor();

    let list = daemon.cli().args(["list"]).output().unwrap();
    let list = String::from_utf8_lossy(&list.stdout);
    assert!(list.contains("keep"), "survivor missing: {list}");
    assert!(
        !list.contains("doomed"),
        "killed session resurrected: {list}"
    );

    unsafe { libc::kill(successor.id() as i32, libc::SIGTERM) };
    let _ = successor.wait();
}

/// Renaming a session updates the persisted list, so a restart restores it under
/// the new name (and not the old).
#[tokio::test]
async fn rename_persists_across_restart() {
    let daemon = Daemon::start("rename");
    assert!(
        daemon
            .cli()
            .args(["new", "before"])
            .status()
            .unwrap()
            .success()
    );

    let mut c = ProtoClient::connect(&daemon.socket).await;
    c.send(Frame::Rename {
        name: "before".into(),
        new_name: "after".into(),
    })
    .await;
    assert!(matches!(c.recv().await, Frame::Ack), "rename not acked");
    drop(c);

    daemon.stop_and_wait();
    let mut successor = daemon.respawn_successor();

    let list = daemon.cli().args(["list"]).output().unwrap();
    let list = String::from_utf8_lossy(&list.stdout);
    assert!(list.contains("after"), "renamed session missing: {list}");
    assert!(!list.contains("before"), "old name still present: {list}");

    unsafe { libc::kill(successor.id() as i32, libc::SIGTERM) };
    let _ = successor.wait();
}

/// `asd list --json`: a machine-readable form of the session table. Always an
/// array — the empty case is `[]`, not the human "no sessions" line — and each
/// object names the session with the same `session` key `inspect --json` uses,
/// so a caller can read either without special-casing.
#[tokio::test]
async fn list_json_is_an_array_of_sessions() {
    let daemon = Daemon::start("listjson");

    // No sessions yet: still valid JSON, not prose.
    let out = daemon.cli().args(["list", "--json"]).output().unwrap();
    assert!(out.status.success(), "list --json failed: {out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "[]");

    for name in ["lj0", "lj1"] {
        assert!(
            daemon
                .cli()
                .args(["new", name])
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    let out = daemon.cli().args(["list", "--json"]).output().unwrap();
    assert!(out.status.success(), "list --json failed: {out:?}");
    let json = String::from_utf8_lossy(&out.stdout);
    let json = json.trim();
    assert!(json.starts_with('[') && json.ends_with(']'), "json: {json}");
    assert!(json.contains(r#""session":"lj0""#), "json: {json}");
    assert!(json.contains(r#""session":"lj1""#), "json: {json}");
    // Both objects are present, comma-separated at the top level.
    assert_eq!(json.matches(r#"{"session":"#).count(), 2, "json: {json}");
    // Default create size, and the fields the table shows.
    assert!(json.contains(r#""cols":80"#), "json: {json}");
    assert!(json.contains(r#""rows":24"#), "json: {json}");
    assert!(json.contains(r#""attached_clients":0"#), "json: {json}");
    assert!(json.contains(r#""status":"#), "json: {json}");

    // Without --json the human table is unchanged.
    let out = daemon.cli().args(["list"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("NAME"), "table: {text}");
    assert!(!text.contains(r#""session":"#), "table leaked json: {text}");
}

/// A missing session must read the same whichever way `wait` was asked to
/// watch it. `--text` reaches the daemon through `Peek`, which answers
/// `Error{NO_SUCH_SESSION}`; `--idle` polls `ListSessions`, which cannot fail on
/// a name it simply does not contain, so the CLI detects the absence itself —
/// and used to word it differently and drop the protocol code, leaving scripts
/// no single pattern to match.
#[tokio::test]
async fn wait_reports_a_missing_session_the_same_way_in_both_modes() {
    let daemon = Daemon::start("waitmissing");

    let by_text = daemon
        .cli()
        .args(["wait", "ghost", "--text", "x", "--timeout", "1s"])
        .output()
        .unwrap();
    let by_idle = daemon
        .cli()
        .args(["wait", "ghost", "--idle", "--timeout", "1s"])
        .output()
        .unwrap();

    for (label, out) in [("--text", &by_text), ("--idle", &by_idle)] {
        assert!(!out.status.success(), "{label} should fail: {out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("wait failed (2)") && err.contains("no such session 'ghost'"),
            "{label} wording: {err}"
        );
    }

    // Not the timeout path: that exits 4 and says so.
    assert_ne!(by_idle.status.code(), Some(4), "idle took the timeout path");
    assert_ne!(by_text.status.code(), Some(4), "text took the timeout path");
}

/// The recorded cwd converges on where the session actually is.
///
/// A shell told to `cd` has not moved yet when the daemon samples its cwd at
/// create time, so the entry starts out recording the daemon's own directory.
/// It used to stay wrong until some unrelated session was added or removed, or
/// until a clean shutdown — a crash in between persisted the wrong directory,
/// and a restart put the session back in the wrong place.
#[tokio::test]
async fn persisted_cwd_follows_the_session() {
    let daemon = Daemon::start("cwdrefresh");
    let target = daemon.dir.join("workdir");
    std::fs::create_dir_all(&target).unwrap();

    assert!(
        daemon
            .cli()
            .args([
                "new",
                "wanderer",
                "--cmd",
                &format!("cd {} && exec bash", target.display()),
            ])
            .output()
            .unwrap()
            .status
            .success()
    );

    let list = daemon.dir.join("data/asd/sessions.tsv");
    let recorded = |()| std::fs::read_to_string(&list).unwrap_or_default();

    // Converges on the shell's real directory without anything else happening.
    let want = target.canonicalize().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !recorded(()).contains(want.to_str().unwrap()) {
        assert!(
            std::time::Instant::now() < deadline,
            "cwd never converged; file: {}",
            recorded(())
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // And keeps following when the session moves again.
    assert!(
        daemon
            .cli()
            .args(["send", "wanderer", "--text", "cd /tmp", "--enter"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !recorded(()).contains("\t/tmp") {
        assert!(
            std::time::Instant::now() < deadline,
            "cwd did not follow the second move; file: {}",
            recorded(())
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Exit statuses a caller can branch on without matching stderr wording.
///
/// Every failure used to be exit 1, with the protocol code surviving only as
/// text in the message — so a script asking "did that session exist?" had to
/// grep. `3` now means it did not, from whichever command asked.
#[tokio::test]
async fn exit_status_distinguishes_a_missing_session() {
    let daemon = Daemon::start("exitcodes");
    assert!(
        daemon
            .cli()
            .args(["new", "live"])
            .output()
            .unwrap()
            .status
            .success()
    );

    // Every command that names a session agrees on 3.
    for args in [
        vec!["peek", "ghost"],
        vec!["send", "ghost", "--text", "x"],
        vec!["inspect", "ghost"],
        vec!["kill", "ghost"],
        vec!["wait", "ghost", "--idle", "--timeout", "1s"],
        vec!["wait", "ghost", "--text", "x", "--timeout", "1s"],
    ] {
        let out = daemon.cli().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(3), "{args:?} → {out:?}");
    }

    // A timeout is its own status, and is not confused with a missing session.
    let out = daemon
        .cli()
        .args(["wait", "live", "--text", "never-appears", "--timeout", "1s"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "timeout: {out:?}");

    // Success stays 0.
    assert_eq!(
        daemon
            .cli()
            .args(["peek", "live"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(0)
    );
}

/// v8: `new --cwd` starts the session in the given directory, so the recorded
/// workspace is right from the first moment instead of converging later.
#[tokio::test]
async fn new_cwd_starts_the_session_there() {
    let daemon = Daemon::start("newcwd");
    let target = daemon.dir.join("startdir");
    std::fs::create_dir_all(&target).unwrap();
    let want = target.canonicalize().unwrap();

    assert!(
        daemon
            .cli()
            .args(["new", "placed", "--cwd", want.to_str().unwrap()])
            .output()
            .unwrap()
            .status
            .success()
    );

    // Recorded immediately — no waiting for the refresh sweep to correct it.
    let list = daemon.dir.join("data/asd/sessions.tsv");
    let recorded = std::fs::read_to_string(&list).unwrap_or_default();
    assert!(
        recorded.contains(want.to_str().unwrap()),
        "cwd not recorded at create: {recorded}"
    );

    // A directory that cannot be entered fails the create rather than silently
    // starting somewhere else.
    let out = daemon
        .cli()
        .args(["new", "nowhere", "--cwd", "/definitely/not/here"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "bad --cwd should fail: {out:?}");
}

/// v8: `list --json` carries the pid, so a caller reaches the process without
/// an `inspect` round trip per session.
#[tokio::test]
async fn list_json_carries_the_pid() {
    let daemon = Daemon::start("listpid");
    assert!(
        daemon
            .cli()
            .args(["new", "p0"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let out = daemon.cli().args(["list", "--json"]).output().unwrap();
    let json = String::from_utf8_lossy(&out.stdout);
    let pid: u32 = json
        .split(r#""pid":"#)
        .nth(1)
        .and_then(|t| t.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|d| d.parse().ok())
        .unwrap_or_else(|| panic!("no pid in {json}"));
    assert!(pid > 0, "pid should be live: {json}");

    // The same pid `inspect` reports — one source of truth, two ways to reach it.
    let ins = daemon
        .cli()
        .args(["inspect", "p0", "--json"])
        .output()
        .unwrap();
    let ins = String::from_utf8_lossy(&ins.stdout);
    assert!(ins.contains(&format!(r#""pid":{pid}"#)), "inspect: {ins}");
}

/// v8: `kill --all` clears every session in one call.
#[tokio::test]
async fn kill_all_clears_every_session() {
    let daemon = Daemon::start("killall");
    for n in ["k0", "k1", "k2"] {
        assert!(
            daemon
                .cli()
                .args(["new", n])
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    let out = daemon.cli().args(["kill", "--all"]).output().unwrap();
    assert!(out.status.success(), "kill --all: {out:?}");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let list = daemon.cli().args(["list", "--json"]).output().unwrap();
        if String::from_utf8_lossy(&list.stdout).trim() == "[]" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sessions outlived kill --all"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // With nothing left it says so rather than failing.
    let out = daemon.cli().args(["kill", "--all"]).output().unwrap();
    assert!(out.status.success(), "kill --all on empty: {out:?}");

    // A name and --all are mutually exclusive; neither is also rejected.
    assert!(
        !daemon
            .cli()
            .args(["kill"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !daemon
            .cli()
            .args(["kill", "x", "--all"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

/// One pty, many viewers: it is sized to the smallest of them.
///
/// "Last resize wins" let whichever client moved most recently decide, so a
/// small window silently cropped everyone else's view and never gave it back.
/// Taking the minimum is the only rule that does not depend on ordering — and
/// the pty grows again the moment the small window leaves.
#[tokio::test]
async fn pty_follows_the_smallest_attached_client() {
    let daemon = Daemon::start("sizeneg");
    assert!(
        daemon
            .cli()
            .args(["new", "shared"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let size = |d: &Daemon| {
        let out = d.cli().args(["list", "--json"]).output().unwrap();
        let json = String::from_utf8_lossy(&out.stdout).to_string();
        let pick = |key: &str| -> u16 {
            json.split(&format!("\"{key}\":"))
                .nth(1)
                .and_then(|t| t.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|d| d.parse().ok())
                .unwrap_or_else(|| panic!("no {key} in {json}"))
        };
        (pick("cols"), pick("rows"))
    };

    // A wide viewer sets the size on its own.
    let mut wide = ProtoClient::connect(&daemon.socket).await;
    wide.attach_sized("shared", 180, 50).await;
    wait_for(
        || size(&daemon) == (180, 50),
        "pty to follow the only client",
    )
    .await;

    // A narrower one joins: everyone drops to the smaller box, per axis.
    let mut narrow = ProtoClient::connect(&daemon.socket).await;
    narrow.attach_sized("shared", 100, 60).await;
    wait_for(
        || size(&daemon) == (100, 50),
        "pty to take the minimum of both",
    )
    .await;

    // A later resize by the wide client cannot force the narrow one out of view.
    wide.send(Frame::Resize {
        cols: 200,
        rows: 70,
    })
    .await;
    wait_for(
        || size(&daemon) == (100, 60),
        "pty to stay within the narrow client",
    )
    .await;

    // The narrow client leaves: the pty grows back to what is left.
    narrow.send(Frame::Detach).await;
    wait_for(
        || size(&daemon) == (200, 70),
        "pty to grow back after the small window closes",
    )
    .await;
}

/// `asd ui` views are exclusive without changing the shared `asd attach`
/// contract. A second TUI revokes the first, removes its size from negotiation,
/// and invalidates its input capability; an ordinary CLI attach stays live.
#[tokio::test]
async fn second_tui_revokes_the_first_but_keeps_cli_attach_shared() {
    let daemon = Daemon::start("tuirevoke");
    assert!(
        daemon
            .cli()
            .args(["new", "shared"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let mut invalid_tui = ProtoClient::connect_kind(&daemon.socket, ClientKind::Tui).await;
    invalid_tui
        .send(Frame::Attach {
            name: "shared".into(),
            cols: 80,
            rows: 24,
            view_id: 0,
            appearance: TerminalAppearance::default(),
        })
        .await;
    assert!(matches!(
        invalid_tui.recv().await,
        Frame::Error {
            code: asd_proto::code::BAD_HANDSHAKE,
            ..
        }
    ));

    let size = |d: &Daemon| {
        let out = d.cli().args(["list", "--json"]).output().unwrap();
        let json = String::from_utf8_lossy(&out.stdout).to_string();
        let pick = |key: &str| -> u16 {
            json.split(&format!("\"{key}\":"))
                .nth(1)
                .and_then(|text| text.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|digits| digits.parse().ok())
                .unwrap_or_else(|| panic!("no {key} in {json}"))
        };
        (pick("cols"), pick("rows"))
    };

    let mut cli = ProtoClient::connect(&daemon.socket).await;
    cli.attach_sized("shared", 200, 70).await;
    let mut first = ProtoClient::connect_kind(&daemon.socket, ClientKind::Tui).await;
    first.attach_sized("shared", 90, 30).await;
    wait_for(
        || size(&daemon) == (90, 30),
        "first TUI to constrain the shared pty",
    )
    .await;

    let mut second = ProtoClient::connect_kind(&daemon.socket, ClientKind::Tui).await;
    second.attach_sized("shared", 150, 50).await;
    assert_eq!(
        first.recv_skipping_output().await,
        Frame::ViewRevoked {
            name: "shared".into(),
            view_id: 1,
        }
    );
    wait_for(
        || size(&daemon) == (150, 50),
        "revoked TUI size to leave negotiation",
    )
    .await;

    first.send(Frame::Resize { cols: 60, rows: 20 }).await;
    tokio::time::sleep(TICK).await;
    assert_eq!(size(&daemon), (150, 50));

    first
        .send(Frame::Input {
            bytes: b"printf 'REVOKED_BAD\\n'\r".to_vec(),
        })
        .await;
    cli.send(Frame::Input {
        bytes: b"printf 'SHARED_OK\\n'\r".to_vec(),
    })
    .await;
    let output = cli.read_output_until(b"SHARED_OK").await;
    assert!(contains(&output, b"SHARED_OK"), "output: {output:?}");
    assert!(!contains(&output, b"REVOKED_BAD"), "output: {output:?}");

    first.attach_sized("shared", 120, 40).await;
    assert_eq!(
        second.recv_skipping_output().await,
        Frame::ViewRevoked {
            name: "shared".into(),
            view_id: 1,
        }
    );
}

/// A killed `asd attach` still hands the terminal back.
///
/// `attach` turns on mouse tracking (SGR 1002/1006, plus whatever the session
/// mirrors) and the alternate screen. Those are undone by a `Drop` guard, and
/// `Drop` does not run when the process is killed — so a closed tab (SIGHUP) or
/// a `kill` from elsewhere (SIGTERM) used to leave the terminal reporting every
/// mouse move as `ESC[<..M` text at the shell prompt. The same hole was closed
/// in `asd ui` before; this pins it shut for `attach`.
#[test]
fn killed_attach_restores_the_terminal() {
    let daemon = Daemon::start("attachsignal");
    assert!(
        daemon
            .cli()
            .args(["new", "term"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let (master, slave_path) = open_pty();
    let mut child = daemon.cli();
    child.args(["attach", "term"]);
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&slave_path)
        .unwrap();
    let mut child = attach_to_pty(child, slave).spawn().unwrap();

    // Read the pty in the background: `attach` writes its setup, then (with the
    // fix) the restore sequence as it dies.
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let reader = {
        let seen = seen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    break; // EIO once the last slave fd closes, i.e. the child is gone
                }
                seen.lock().unwrap().extend_from_slice(&buf[..n as usize]);
            }
            unsafe { libc::close(master) };
        })
    };
    let saw = |needle: &[u8]| contains(&seen.lock().unwrap(), needle);

    // Wait until it has taken the terminal over (mouse tracking on).
    let deadline = std::time::Instant::now() + WAIT;
    while !saw(b"\x1b[?1002h") {
        assert!(
            std::time::Instant::now() < deadline,
            "attach never enabled mouse tracking: {:?}",
            String::from_utf8_lossy(&seen.lock().unwrap())
        );
        std::thread::sleep(TICK);
    }

    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let _ = child.wait();
    reader.join().unwrap();

    let out = seen.lock().unwrap().clone();
    let dump = String::from_utf8_lossy(&out).into_owned();
    for off in [b"\x1b[?1002l".as_slice(), b"\x1b[?1006l", b"\x1b[?1049l"] {
        assert!(
            contains(&out, off),
            "terminal left in {:?} after SIGTERM; pty saw: {dump:?}",
            String::from_utf8_lossy(off)
        );
    }
}

/// A killed `asd ui` must close the host terminal's synchronized-update mode
/// before restoring mouse/paste/alternate-screen state. Normal frames already
/// contain `?2026l`, so inspect only bytes emitted after a quiet pre-kill
/// boundary; otherwise a completed frame could make the assertion pass while
/// the signal handler itself still omitted the close.
#[test]
fn killed_ui_closes_synchronized_update_before_restoring_terminal() {
    use std::os::unix::process::ExitStatusExt;

    let mut daemon = Daemon::start("uisignal");
    let (master, slave_path) = open_pty();
    let mut command = daemon.cli();
    command.arg("ui");
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&slave_path)
        .unwrap();
    let mut child = attach_to_pty(command, slave).spawn().unwrap();

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let reader = {
        let seen = seen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    break;
                }
                seen.lock().unwrap().extend_from_slice(&buf[..n as usize]);
            }
            unsafe { libc::close(master) };
        })
    };

    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let output = seen.lock().unwrap();
        if contains(&output, b"\x1b[?1002h")
            && contains(&output, b"\x1b[?2026h")
            && contains(&output, b"\x1b[?2026l")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ui never completed its first frame: {:?}",
            String::from_utf8_lossy(&output)
        );
        drop(output);
        std::thread::sleep(TICK);
    }

    // Find a quiet interval between the 1.5 s session-list polls, then record
    // the boundary immediately before SIGTERM. This makes the checked suffix
    // signal-handler output rather than an earlier normal frame.
    let mut last_len = seen.lock().unwrap().len();
    let mut stable_since = std::time::Instant::now();
    loop {
        std::thread::sleep(TICK);
        let len = seen.lock().unwrap().len();
        if len != last_len {
            last_len = len;
            stable_since = std::time::Instant::now();
        }
        if stable_since.elapsed() >= Duration::from_millis(250) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ui output never settled before SIGTERM"
        );
    }
    let kill_offset = seen.lock().unwrap().len();

    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = child.wait().unwrap();
    reader.join().unwrap();

    assert_eq!(status.signal(), Some(libc::SIGTERM));
    assert!(
        daemon.child.try_wait().unwrap().is_none(),
        "killing the ui also stopped its daemon"
    );
    let out = seen.lock().unwrap().clone();
    let suffix = &out[kill_offset..];
    let restore = b"\x1b[?2026l\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?1049l\x1b[?25h\x1b[0m";
    assert!(
        suffix.starts_with(restore),
        "ui did not emit a complete ordered restore after SIGTERM; post-kill bytes: {:?}",
        String::from_utf8_lossy(suffix)
    );
    for off in [
        b"\x1b[?1002l".as_slice(),
        b"\x1b[?2004l",
        b"\x1b[?1049l",
        b"\x1b[?25h",
        b"\x1b[0m",
    ] {
        assert!(
            contains(suffix, off),
            "ui terminal restore omitted {:?}; post-kill bytes: {:?}",
            String::from_utf8_lossy(off),
            String::from_utf8_lossy(suffix)
        );
    }
}

/// Two real `asd ui` processes exercise the user-facing half of TUI takeover:
/// the displaced process stays alive, clears the terminal pane, and paints the
/// asd wordmark plus an actionable message.
#[test]
fn displaced_ui_shows_the_takeover_placard() {
    let daemon = Daemon::start("uiplacard");
    assert!(
        daemon
            .cli()
            .args(["new", "shared"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let spawn_ui = |tag: &str, session: &str| {
        use std::os::fd::AsRawFd;

        let (master, slave_path) = open_pty();
        let window = libc::winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&slave_path)
            .unwrap();
        assert_eq!(
            unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &window) },
            0,
            "setting {tag} pty size failed"
        );
        let mut command = daemon.cli();
        command.args(["ui", session]);
        let child = attach_to_pty(command, slave).spawn().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let reader = {
            let seen = seen.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
                    if n <= 0 {
                        break;
                    }
                    seen.lock().unwrap().extend_from_slice(&buf[..n as usize]);
                }
                unsafe { libc::close(master) };
            })
        };
        (child, seen, reader)
    };

    let (mut first, first_output, first_reader) = spawn_ui("first", "shared");
    let attached = || {
        let out = daemon.cli().args(["list", "--json"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).contains("\"attached_clients\":1")
    };
    let deadline = std::time::Instant::now() + WAIT;
    while !attached() {
        assert!(
            std::time::Instant::now() < deadline,
            "first ui never attached"
        );
        std::thread::sleep(TICK);
    }

    assert!(
        daemon
            .cli()
            .args(["rename", "shared", "renamed"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let (mut second, _second_output, second_reader) = spawn_ui("second", "renamed");
    let deadline = std::time::Instant::now() + WAIT;
    let saw_placard = loop {
        let output = first_output.lock().unwrap();
        if contains(&output, b"__ _ ___")
            && contains(&output, b"Session \"renamed\" is open in another asd ui")
            && contains(&output, b"Select it again to take over")
        {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        drop(output);
        std::thread::sleep(TICK);
    };

    unsafe {
        libc::kill(first.id() as i32, libc::SIGTERM);
        libc::kill(second.id() as i32, libc::SIGTERM);
    }
    let _ = first.wait();
    let _ = second.wait();
    first_reader.join().unwrap();
    second_reader.join().unwrap();

    assert!(
        saw_placard,
        "displaced ui output: {:?}",
        String::from_utf8_lossy(&first_output.lock().unwrap())
    );
}

/// A master pty and the path of its slave.
fn open_pty() -> (libc::c_int, PathBuf) {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0, "posix_openpt failed");
        assert_eq!(libc::grantpt(master), 0, "grantpt failed");
        assert_eq!(libc::unlockpt(master), 0, "unlockpt failed");
        let path = {
            let _guard = PTSNAME_LOCK.lock().unwrap();
            let name_ptr = libc::ptsname(master);
            assert!(!name_ptr.is_null(), "ptsname failed");
            std::ffi::CStr::from_ptr(name_ptr)
                .to_string_lossy()
                .into_owned()
        };
        (master, PathBuf::from(path))
    }
}

/// Run `cmd` with `slave` as its controlling terminal, the way a shell would.
fn attach_to_pty(mut cmd: Command, slave: std::fs::File) -> Command {
    use std::os::unix::process::CommandExt;

    cmd.stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    unsafe {
        // Between fork and exec: async-signal-safe calls only. The slave is
        // already on fd 0 by now, so that is what to claim as the terminal.
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // TIOCSCTTY and the ioctl request parameter do not share a type
            // on every Unix. The conversion is load-bearing on macOS and an
            // identity on Linux, where clippy would otherwise reject it.
            #[allow(clippy::useless_conversion)]
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY.into(), 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

/// A session's child is pointed at the daemon that hosts it. A daemon serving a
/// non-default `--socket` hands that exact path down as `$ASD_SOCKET`, so an
/// `asd` command run inside a session addresses its own daemon instead of
/// resolving the default path and answering for a different one.
#[tokio::test]
async fn session_children_are_given_the_hosting_daemons_socket() {
    let daemon = Daemon::start("sessionenv");
    assert!(
        daemon
            .cli()
            .args(["new", "envs"])
            .output()
            .unwrap()
            .status
            .success()
    );

    // Ask the session's own shell. The marker is assembled by printf so the
    // echoed input line cannot satisfy the wait — only the output can.
    let probe = format!(
        "[ \"$ASD_SOCKET\" = \"{}\" ] && printf 'ASD_SOCKET_%s\\n' MATCHES",
        daemon.socket.display()
    );
    let out = daemon
        .cli()
        .args(["send", "envs", "--text", &probe, "--enter"])
        .output()
        .unwrap();
    assert!(out.status.success(), "send failed: {out:?}");

    let out = daemon
        .cli()
        .args([
            "wait",
            "envs",
            "--text",
            "ASD_SOCKET_MATCHES",
            "--timeout",
            "10s",
        ])
        .output()
        .unwrap();
    let screen = daemon.cli().args(["peek", "envs"]).output().unwrap();
    assert!(
        out.status.success(),
        "session did not see the daemon socket in $ASD_SOCKET; screen:\n{}",
        String::from_utf8_lossy(&screen.stdout)
    );
}

/// The daemon reads the screen of a recognized agent and reports what it says,
/// end to end: rules → session thread → `SessionInfo.state` → `asd list`.
///
/// The session runs a shell script rather than a real agent — the point under
/// test is the daemon's plumbing, not any agent's UI, and the rules themselves
/// are covered against captured screens in asd-daemon. It prints a screen that
/// Claude Code's rules classify, and renames itself to `claude` so the
/// foreground-command lookup resolves the manifest.
#[tokio::test]
async fn the_daemon_reports_a_recognized_agents_screen_state() {
    let daemon = Daemon::start("agentstate");

    // exec through a copy named `claude`, so /proc reports that as the pty's
    // foreground command — which is how the daemon picks the rule set.
    let fake = daemon.dir.join("claude");
    std::fs::copy("/bin/sh", &fake).unwrap();
    let script = format!(
        "exec {} -c 'printf \"\\033]0;\\u2733 asd\\007\";          printf \"Do you want to proceed?\\r\\n\";          printf \"1. Yes\\r\\n2. No\\r\\n\"; sleep 60'",
        fake.display()
    );
    let out = daemon
        .cli()
        .args(["new", "agent", "--cmd", &script])
        .output()
        .unwrap();
    assert!(out.status.success(), "new failed: {out:?}");

    // Detection runs on the session thread behind a throttle, so the state
    // appears shortly after the screen does rather than with it.
    let status = |daemon: &Daemon| -> String {
        let out = daemon.cli().args(["inspect", "agent", "--json"]).output();
        out.map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default()
    };
    wait_for(
        || status(&daemon).contains(r#""status":"blocked""#),
        "the daemon to report the agent as blocked",
    )
    .await;

    // `list` renders the same reading in its STATUS column.
    let out = daemon.cli().args(["list"]).output().unwrap();
    let listing = String::from_utf8_lossy(&out.stdout);
    assert!(
        listing
            .lines()
            .any(|l| l.starts_with("agent") && l.contains("blocked")),
        "list did not show the state:\n{listing}"
    );

    // And `wait --until` returns on it without polling from the script.
    let out = daemon
        .cli()
        .args(["wait", "agent", "--until", "blocked", "--timeout", "10s"])
        .output()
        .unwrap();
    assert!(out.status.success(), "wait --until blocked failed: {out:?}");
}

/// `send-all` types into every session, skips the one it is running in, and
/// reports what it did.
#[tokio::test]
async fn send_all_types_into_every_session_but_its_own() {
    let daemon = Daemon::start("sendall");
    for name in ["one", "two", "three"] {
        assert!(
            daemon
                .cli()
                .args(["new", name])
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    // --dry-run names the targets without writing: for a command that types
    // into every live session at once, seeing the list first is the point.
    let out = daemon
        .cli()
        .args(["send-all", "--text", "x", "--dry-run"])
        .env("ASD_SESSION", "two")
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "dry run failed: {out:?}");
    assert!(
        listed.contains("one") && listed.contains("three"),
        "{listed}"
    );
    assert!(
        !listed.contains("\n  two"),
        "the caller's own session was listed as a target:\n{listed}"
    );

    // The screens are untouched by a dry run.
    let screen = daemon.cli().args(["peek", "one"]).output().unwrap();
    assert!(
        !String::from_utf8_lossy(&screen.stdout).contains("sendallmark"),
        "dry run wrote to a session"
    );

    let out = daemon
        .cli()
        .args(["send-all", "--text", "echo sendallmark-$((6*7))", "--enter"])
        .env("ASD_SESSION", "two")
        .output()
        .unwrap();
    assert!(out.status.success(), "send-all failed: {out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("sent to 2/2"),
        "unexpected summary: {out:?}"
    );

    for name in ["one", "three"] {
        let out = daemon
            .cli()
            .args(["wait", name, "--text", "sendallmark-42", "--timeout", "10s"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{name} never got the payload: {out:?}"
        );
    }

    // And the skipped session really was skipped, not merely absent from the
    // summary.
    let screen = daemon.cli().args(["peek", "two"]).output().unwrap();
    assert!(
        !String::from_utf8_lossy(&screen.stdout).contains("sendallmark"),
        "the caller's own session was written to"
    );
}

/// Poll `cond` until it holds, or fail with `what`.
async fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
