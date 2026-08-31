//! Daemon lifecycle: restart and successor hand-off, SIGTERM cleanup, the
//! --stdio proxy, version rejection, and everything restored from sessions.tsv.

use std::process::{Command, Stdio};
use std::time::Duration;

use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, PROTO_VERSION, code};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::time::timeout;

use crate::common::*;

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

/// The daemon answers a metrics request out of its sampler's stored reading.
/// `sample: None` is a real answer only for the sampler's first second after
/// start-up -- past that, it must turn into `Some`, or the sampler never ran
/// at all (e.g. `server::serve` forgot to spawn it) and the bar would show
/// nothing forever. So this polls for `Some`, with a bounded timeout, rather
/// than accepting `None` as a pass. What must not happen otherwise is an
/// error, a wrong frame, or a hang.
#[tokio::test]
async fn host_metrics_are_served_from_the_daemon() {
    let daemon = Daemon::start("host-metrics");
    let mut c = ProtoClient::connect(&daemon.socket).await;

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let sample = loop {
        c.send(Frame::HostMetrics).await;
        match c.recv().await {
            Frame::HostMetricsReply { sample: Some(s) } => break s,
            Frame::HostMetricsReply { sample: None } => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for the sampler to store a reading"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            other => panic!("expected HostMetricsReply, got {other:?}"),
        }
    };
    // u8 is unsigned, so only the upper bound is worth asserting.
    assert!(
        sample.cpu_pct <= 100,
        "cpu out of range: {}",
        sample.cpu_pct
    );
    assert!(
        sample.mem_total_bytes > 0,
        "a host with no memory is not real"
    );
    assert!(sample.mem_used_bytes <= sample.mem_total_bytes);
}

/// A restored session brings its command back to the prompt, and leaves it
/// there. Re-running an arbitrary command on every daemon restart is not
/// something a mux may decide by itself — the recorded command could be a
/// deploy or a migration — so the restore types it and waits for a person.
#[tokio::test]
async fn restart_stages_the_recorded_command_without_running_it() {
    let daemon = Daemon::start("stagecmd");
    let marker = daemon.dir.join("it-ran");
    let command = format!("touch '{}'; sleep 300", marker.display());

    assert!(
        daemon
            .cli()
            .args(["new", "job", "--cmd", &command])
            .status()
            .unwrap()
            .success(),
        "create failed"
    );
    // A plain shell session alongside it: the common case, which must keep
    // restoring exactly as before.
    assert!(
        daemon
            .cli()
            .args(["new", "plain"])
            .status()
            .unwrap()
            .success()
    );
    // The create itself runs the command, as it always has.
    wait_for(|| marker.exists(), "the created session to run its command").await;

    // The persisted list records the command as a third field, and records
    // nothing there for the shell session.
    let state_path = daemon.dir.join("data/asd/sessions.tsv");
    wait_for(
        || {
            std::fs::read_to_string(&state_path)
                .map(|t| {
                    t.lines()
                        .any(|l| l.starts_with("job\t") && l.contains("touch"))
                })
                .unwrap_or(false)
        },
        "the command to reach the session list",
    )
    .await;
    let state = std::fs::read_to_string(&state_path).unwrap();
    let plain = state
        .lines()
        .find(|l| l.starts_with("plain\t"))
        .expect("shell session missing from the list");
    assert_eq!(
        plain.splitn(3, '\t').nth(2),
        Some(""),
        "a shell session must record no command, got: {plain:?}"
    );

    // Clear the evidence, so anything that appears after the restart is a
    // second run and not the first one's leftovers.
    std::fs::remove_file(&marker).unwrap();

    daemon.stop_and_wait();
    let mut successor = daemon.respawn_successor();

    let list = daemon.cli().args(["list"]).output().unwrap();
    let list = String::from_utf8_lossy(&list.stdout);
    assert!(list.contains("job"), "session not restored: {list}");

    // The command is on the prompt line...
    wait_for(
        || {
            let out = daemon.cli().args(["peek", "job"]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).contains("sleep 300")
        },
        "the recorded command to be typed at the restored prompt",
    )
    .await;
    // ...and it did not run.
    assert!(
        !marker.exists(),
        "the restored command ran without being confirmed"
    );

    // Enter is the confirmation.
    assert!(
        daemon
            .cli()
            .args(["send", "job", "--key", "Enter"])
            .status()
            .unwrap()
            .success()
    );
    wait_for(|| marker.exists(), "the staged command to run on Enter").await;

    unsafe { libc::kill(successor.id() as i32, libc::SIGTERM) };
    let _ = successor.wait();
}

/// The escape hatch: a daemon started with `--run-restored-commands` runs each
/// restored command instead of waiting at the prompt.
#[tokio::test]
async fn run_restored_commands_runs_them_without_confirmation() {
    let daemon = Daemon::start("runcmd");
    let marker = daemon.dir.join("it-ran");
    let command = format!("touch '{}'; sleep 300", marker.display());

    assert!(
        daemon
            .cli()
            .args(["new", "job", "--cmd", &command])
            .status()
            .unwrap()
            .success(),
        "create failed"
    );
    wait_for(|| marker.exists(), "the created session to run its command").await;
    std::fs::remove_file(&marker).unwrap();

    daemon.stop_and_wait();
    let mut successor = daemon.respawn_successor_with(&["--run-restored-commands"]);

    // No Enter is sent: the flag supplies it.
    wait_for(
        || marker.exists(),
        "the restored command to run on its own under --run-restored-commands",
    )
    .await;

    unsafe { libc::kill(successor.id() as i32, libc::SIGTERM) };
    let _ = successor.wait();
}
