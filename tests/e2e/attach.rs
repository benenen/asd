//! Attach-path tests: snapshots and broadcast, terminal queries, PTY size
//! negotiation, read-only clients, TUI takeover, and end-of-session reporting.

use std::time::Duration;

use asd_proto::{ClientKind, Frame, TerminalAppearance, TerminalColor, code};

use crate::common::*;

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
            read_only: false,
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
            read_only: false,
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
        read_only: false,
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
            read_only: false,
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

/// A read-only client sees everything and reaches nothing. Its keystrokes are
/// dropped by the daemon rather than written to the pty, while output keeps
/// flowing to it — the point of the mode is watching an agent without being one
/// keystroke away from derailing it.
#[tokio::test]
async fn a_read_only_client_watches_but_cannot_type() {
    let daemon = Daemon::start("readonly");
    assert!(
        daemon
            .cli()
            .args(["new", "watched"])
            .status()
            .unwrap()
            .success()
    );

    let watcher_marker = daemon.dir.join("watcher-typed");
    let sender_marker = daemon.dir.join("sender-typed");

    let mut watcher = ProtoClient::connect(&daemon.socket).await;
    watcher.attach_read_only("watched", 80, 24).await;

    // The watcher types a command that would leave a file behind.
    watcher
        .send(Frame::Input {
            bytes: format!("touch '{}'\r", watcher_marker.display()).into_bytes(),
        })
        .await;

    // Then a command goes in through a channel that is allowed to write. When
    // its file appears, the pty has processed input that arrived *after* the
    // watcher's — so the watcher's absence below is a fact, not a race.
    assert!(
        daemon
            .cli()
            .args([
                "send",
                "watched",
                "--text",
                &format!("touch '{}'", sender_marker.display()),
                "--enter",
            ])
            .status()
            .unwrap()
            .success()
    );
    wait_for(|| sender_marker.exists(), "the writable client's command").await;
    assert!(
        !watcher_marker.exists(),
        "a read-only client's input reached the pty"
    );

    // ...and the watcher is still a viewer: it receives the output of what the
    // other client did.
    let seen = watcher.read_output_until(b"touch").await;
    assert!(
        String::from_utf8_lossy(&seen).contains("touch"),
        "watcher stopped receiving output"
    );
}

/// A watcher's window is not the session's business: attaching read-only at a
/// smaller size leaves the pty where the typing clients put it, and a Resize
/// from that client changes nothing either.
#[tokio::test]
async fn a_read_only_client_does_not_resize_the_session() {
    let daemon = Daemon::start("rosize");
    assert!(
        daemon
            .cli()
            .args(["new", "sized"])
            .status()
            .unwrap()
            .success()
    );

    // One ordinary client sets the size.
    let mut typer = ProtoClient::connect(&daemon.socket).await;
    typer.attach_sized("sized", 100, 30).await;
    wait_for(
        || {
            let out = daemon.cli().args(["list"]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).contains("100x30")
        },
        "the attached client's size to take effect",
    )
    .await;

    // A much smaller watcher joins. A read-write client this size would drag
    // the pty down to 20x5.
    let mut watcher = ProtoClient::connect(&daemon.socket).await;
    watcher.attach_read_only("sized", 20, 5).await;
    watcher.send(Frame::Resize { cols: 20, rows: 5 }).await;

    // Give both a chance to be wrong, then assert they were not.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let out = daemon.cli().args(["list"]).output().unwrap();
    let list = String::from_utf8_lossy(&out.stdout);
    assert!(
        list.contains("100x30"),
        "a read-only client resized the session: {list}"
    );
}

/// `follow --json` ends with how the child ended, not just that it did. A
/// session that exits 7 and one that is killed are different outcomes, and
/// until now both arrived as a bare `exit` event.
#[tokio::test]
async fn follow_reports_how_the_session_ended() {
    let daemon = Daemon::start("exitcode");

    // The signal's *name* is the platform's own wording (`Killed`, not
    // `SIGKILL`), so assert on the shape rather than on glibc's vocabulary.
    for (name, command, expected) in [
        ("bycode", "sleep 1; exit 7", r#""code":7,"signal":null"#),
        ("bysignal", "sleep 1; kill -9 $$", r#""code":1,"signal":""#),
    ] {
        assert!(daemon.cli().args(["new", name]).status().unwrap().success());
        // The leading sleep is what lets `follow` subscribe before the session
        // ends, the same trick the other follow tests use.
        assert!(
            daemon
                .cli()
                .args(["send", name, "--text", command, "--enter"])
                .status()
                .unwrap()
                .success()
        );

        let out = daemon
            .cli()
            .args(["follow", name, "--forever", "--json", "--timeout", "20s"])
            .output()
            .unwrap();
        assert!(out.status.success(), "follow failed: {out:?}");
        let streamed = String::from_utf8_lossy(&out.stdout);
        let exit_line = streamed
            .lines()
            .find(|l| l.contains(r#""event":"exit""#))
            .unwrap_or_else(|| panic!("no exit event for {name}: {streamed}"));
        assert!(
            exit_line.contains(expected),
            "{name}: expected {expected} in {exit_line}"
        );
    }
}

/// The same fact reaches an attached client, which has only the message to go
/// on: it now names the status or the signal instead of saying only "exited".
#[tokio::test]
async fn an_attached_client_is_told_how_the_session_ended() {
    let daemon = Daemon::start("exitmsg");
    assert!(
        daemon
            .cli()
            .args(["new", "doomed"])
            .status()
            .unwrap()
            .success()
    );

    let mut c = ProtoClient::connect(&daemon.socket).await;
    c.attach("doomed").await;

    // `asd kill` is SIGHUP, and a shell dies by it.
    assert!(
        daemon
            .cli()
            .args(["kill", "doomed"])
            .status()
            .unwrap()
            .success()
    );

    let ended = loop {
        match c.recv().await {
            Frame::Error { code, msg } if code == asd_proto::code::SESSION_EXITED => break msg,
            Frame::Output { .. } | Frame::FollowStatus { .. } => {}
            other => panic!("unexpected frame while waiting for the ending: {other:?}"),
        }
    };
    assert!(
        ended.contains("(signal ") || ended.contains("(status "),
        "the ending should say how it ended, got: {ended}"
    );
}
