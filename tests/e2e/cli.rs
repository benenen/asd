//! `asd` subcommands over a session list: list, kill, rename, inspect, new
//! --cwd, JSON output, and exit statuses. Includes the scripted-daemon tests,
//! which drive the CLI against a hand-written socket instead of a real daemon.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, PROTO_VERSION, code};
use tokio::net::UnixListener;
use tokio::time::timeout;

use crate::common::*;

struct ScriptedSocket {
    dir: PathBuf,
    path: PathBuf,
}

impl ScriptedSocket {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "asd-scripted-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("asd.sock");
        Self { dir, path }
    }
}

impl Drop for ScriptedSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn accept_cli(
    listener: &UnixListener,
) -> (
    FrameReader<tokio::net::unix::OwnedReadHalf>,
    FrameWriter<tokio::net::unix::OwnedWriteHalf>,
) {
    let (stream, _) = timeout(WAIT, listener.accept())
        .await
        .expect("CLI connect timeout")
        .expect("CLI connect failed");
    let (read, write) = stream.into_split();
    let mut reader = FrameReader::new(read);
    let mut writer = FrameWriter::new(write);
    assert_eq!(
        scripted_recv(&mut reader).await,
        Some(Frame::Hello {
            proto_version: PROTO_VERSION,
            kind: ClientKind::Cli,
        })
    );
    writer
        .write_frame(&Frame::HelloAck {
            proto_version: PROTO_VERSION,
            daemon_version: "test".to_string(),
        })
        .await
        .unwrap();
    (reader, writer)
}

async fn scripted_recv(reader: &mut FrameReader<tokio::net::unix::OwnedReadHalf>) -> Option<Frame> {
    timeout(WAIT, reader.read_frame())
        .await
        .expect("scripted frame timeout")
        .expect("scripted frame read failed")
}

fn scripted_session(name: &str, instance_id: u128) -> asd_proto::SessionInfo {
    asd_proto::SessionInfo {
        name: name.to_string(),
        instance_id,
        command: "sh".to_string(),
        title: String::new(),
        status_line: String::new(),
        created_ms: 100,
        idle_ms: 0,
        running: false,
        state: asd_proto::AgentState::Unknown,
        attached_clients: 0,
        pid: 42,
        cols: 80,
        rows: 24,
    }
}

#[tokio::test]
async fn named_kill_sends_the_identity_from_its_list_snapshot() {
    let socket = ScriptedSocket::new("named-kill-identity");
    let listener = UnixListener::bind(&socket.path).unwrap();
    let child = tokio::process::Command::new(cli_exe())
        .arg("--socket")
        .arg(&socket.path)
        .args(["kill", "web"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut reader, mut writer) = accept_cli(&listener).await;

    assert_eq!(scripted_recv(&mut reader).await, Some(Frame::ListSessions));
    writer
        .write_frame(&Frame::SessionList {
            sessions: vec![scripted_session("web", 0x1234)],
        })
        .await
        .unwrap();
    assert_eq!(
        scripted_recv(&mut reader).await,
        Some(Frame::Kill {
            name: "web".to_string(),
            identity: asd_proto::SessionIdentity {
                instance_id: 0x1234,
            },
        })
    );
    assert_eq!(scripted_recv(&mut reader).await, Some(Frame::ListSessions));
    writer
        .write_frame(&Frame::SessionList { sessions: vec![] })
        .await
        .unwrap();

    let output = timeout(WAIT, child.wait_with_output())
        .await
        .expect("CLI exit timeout")
        .unwrap();
    assert!(output.status.success(), "named kill failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "kill signalled: web"
    );
}

#[tokio::test]
async fn kill_all_treats_a_single_stale_snapshot_as_an_idempotent_race() {
    let socket = ScriptedSocket::new("kill-all-stale");
    let listener = UnixListener::bind(&socket.path).unwrap();
    let child = tokio::process::Command::new(cli_exe())
        .arg("--socket")
        .arg(&socket.path)
        .args(["kill", "--all"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut reader, mut writer) = accept_cli(&listener).await;

    assert_eq!(scripted_recv(&mut reader).await, Some(Frame::ListSessions));
    writer
        .write_frame(&Frame::SessionList {
            sessions: vec![scripted_session("web", 1)],
        })
        .await
        .unwrap();
    assert_eq!(
        scripted_recv(&mut reader).await,
        Some(Frame::Kill {
            name: "web".to_string(),
            identity: asd_proto::SessionIdentity { instance_id: 1 },
        })
    );
    writer
        .write_frame(&Frame::Error {
            code: code::STALE_SESSION,
            msg: "session 'web' changed since it was selected".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(scripted_recv(&mut reader).await, Some(Frame::ListSessions));
    writer
        .write_frame(&Frame::SessionList {
            sessions: vec![scripted_session("web", 2)],
        })
        .await
        .unwrap();

    let output = timeout(WAIT, child.wait_with_output())
        .await
        .expect("CLI exit timeout")
        .unwrap();
    assert!(output.status.success(), "kill --all failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("kill requested: web"), "stdout: {stdout}");
    assert!(!stdout.contains("kill signalled"), "stdout: {stdout}");
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
