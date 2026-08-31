//! Shared harness for the e2e tests: an isolated `Daemon` process, a raw
//! protocol client, and the small waiting helpers every module needs.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, PROTO_VERSION};
use tokio::net::UnixStream;
use tokio::time::timeout;

pub const TICK: Duration = Duration::from_millis(50);
pub const WAIT: Duration = Duration::from_secs(10);

pub fn cli_exe() -> &'static str {
    env!("CARGO_BIN_EXE_asd")
}

/// An isolated daemon instance: its own socket + data directory, reclaimed
/// on Drop.
pub struct Daemon {
    pub child: Child,
    pub socket: PathBuf,
    pub dir: PathBuf,
}

impl Daemon {
    pub fn start(tag: &str) -> Self {
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

    pub fn cli(&self) -> Command {
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
    pub fn child_pids(&self) -> Vec<u32> {
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

    pub fn sigterm(&self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
    }

    /// Start a fresh daemon on the same socket + data dir (as a detached process,
    /// not our child) and wait for it to accept connections. Used to test restore
    /// after the original daemon has stopped. Returns the child so the caller can
    /// SIGTERM it at the end.
    pub fn respawn_successor(&self) -> std::process::Child {
        self.respawn_successor_with(&[])
    }

    /// A successor daemon on the same socket and data dir, with `extra` flags.
    pub fn respawn_successor_with(&self, extra: &[&str]) -> std::process::Child {
        let child = Command::new(cli_exe())
            .arg("daemon")
            .arg("--socket")
            .arg(&self.socket)
            .args(extra)
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
    pub fn stop_and_wait(&self) {
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
    pub fn session_cwd(&self, name: &str) -> Option<PathBuf> {
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
    pub fn wait_session_cwd(&self, name: &str, want: &Path) {
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
pub struct ProtoClient {
    reader: FrameReader<tokio::net::unix::OwnedReadHalf>,
    writer: FrameWriter<tokio::net::unix::OwnedWriteHalf>,
    kind: ClientKind,
    next_view_id: u64,
}

impl ProtoClient {
    pub async fn connect(socket: &Path) -> Self {
        Self::connect_kind(socket, ClientKind::Cli).await
    }

    pub async fn connect_kind(socket: &Path, kind: ClientKind) -> Self {
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

    pub async fn send(&mut self, frame: Frame) {
        timeout(WAIT, self.writer.write_frame(&frame))
            .await
            .expect("write timeout")
            .expect("write failed");
    }

    pub async fn recv(&mut self) -> Frame {
        timeout(WAIT, self.reader.read_frame())
            .await
            .expect("read timeout")
            .expect("read failed")
            .expect("connection closed unexpectedly")
    }

    /// Attach and return the Snapshot contents.
    pub async fn attach(&mut self, name: &str) -> Vec<u8> {
        self.attach_sized(name, 80, 24).await
    }

    /// Attach as a client of a given window size.
    pub async fn attach_sized(&mut self, name: &str, cols: u16, rows: u16) -> Vec<u8> {
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
            read_only: false,
        })
        .await;
        match self.recv().await {
            Frame::Snapshot { vt } => vt,
            other => panic!("expected Snapshot, got {other:?}"),
        }
    }

    /// Attach as a watcher: the daemon must drop this client's input and leave
    /// it out of size negotiation.
    pub async fn attach_read_only(&mut self, name: &str, cols: u16, rows: u16) -> Vec<u8> {
        self.send(Frame::Attach {
            name: name.into(),
            cols,
            rows,
            view_id: 0,
            appearance: asd_proto::TerminalAppearance::default(),
            read_only: true,
        })
        .await;
        match self.recv().await {
            Frame::Snapshot { vt } => vt,
            other => panic!("expected Snapshot, got {other:?}"),
        }
    }

    /// Receive the next frame that is not Output (draining live Output).
    pub async fn recv_skipping_output(&mut self) -> Frame {
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
    pub async fn read_output_until(&mut self, needle: &[u8]) -> Vec<u8> {
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

pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Poll `cond` until it holds, or fail with `what`.
pub async fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
