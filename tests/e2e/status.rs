//! What a session reports about itself: the running flag, the recognized
//! agent screen state, its self-declared status line, and `asd card`.

use std::time::Duration;

use asd_proto::Frame;

use crate::common::*;

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

/// The point of `asd status` is that the program inside a session sets it: the
/// child has `$ASD_SESSION` and `$ASD_SOCKET`, so it can describe itself
/// without being told its own name or where its daemon is. Anything the daemon
/// reads off the screen can say a session is busy; only the session can say it
/// is on step three.
#[tokio::test]
async fn a_session_can_say_what_it_is_doing_from_inside_itself() {
    let daemon = Daemon::start("saysit");
    assert!(
        daemon
            .cli()
            .args(["new", "worker"])
            .status()
            .unwrap()
            .success()
    );

    // Typed into the session, exactly as an agent would run it.
    assert!(
        daemon
            .cli()
            .args([
                "send",
                "worker",
                "--text",
                &format!("{} status --text 'step 3/7: running tests'", cli_exe()),
                "--enter",
            ])
            .status()
            .unwrap()
            .success()
    );

    wait_for(
        || {
            let out = daemon.cli().args(["list"]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).contains("step 3/7")
        },
        "the session's own status line to reach `list`",
    )
    .await;

    // ...and it is a field of its own in JSON, not the activity status.
    let out = daemon.cli().args(["list", "--json"]).output().unwrap();
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        json.contains(r#""says":"step 3/7: running tests""#),
        "says missing from: {json}"
    );

    // Reading it back needs no name from inside; from outside it takes one.
    let out = daemon.cli().args(["status", "worker"]).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "step 3/7: running tests"
    );

    // Clearing hands the column back to the terminal title.
    assert!(
        daemon
            .cli()
            .args(["status", "worker", "--clear"])
            .status()
            .unwrap()
            .success()
    );
    let out = daemon.cli().args(["list", "--json"]).output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(r#""says":"""#),
        "clear left something behind"
    );
}
