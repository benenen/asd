//! Attach-free scripting frames as the CLI exposes them: send, wait, peek,
//! send-all, and ask.

use std::process::Stdio;
use std::time::Duration;

use crate::common::*;

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

/// `wait --idle` returns once output settles; a condition that never holds
/// times out with the documented exit code 4.
#[tokio::test]
async fn wait_idle_and_timeout() {
    let daemon = Daemon::start("waitidle");
    assert!(
        daemon
            .cli()
            .args(["new", "quiet", "--cmd", "exec bash --norc -i"])
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

/// `asd ask` is send-and-wait as one operation: it types, waits for the session
/// to settle, and says where it settled. On a shell that means the command runs
/// and the prompt comes back.
#[tokio::test]
async fn ask_sends_and_waits_for_the_session_to_settle() {
    let daemon = Daemon::start("ask");
    assert!(
        daemon
            .cli()
            .args(["new", "peer"])
            .status()
            .unwrap()
            .success()
    );

    let out = daemon
        .cli()
        .args(["ask", "peer", "echo ASKED-AND-ANSWERED", "--timeout", "20s"])
        .output()
        .unwrap();
    assert!(out.status.success(), "ask failed: {out:?}");
    // It reports where it settled, so a caller can branch without asking again.
    // A plain shell is never classified as an agent, so this is the activity
    // reading — the same word `list` prints.
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "idle");

    // And the text really went in.
    let peek = daemon.cli().args(["peek", "peer"]).output().unwrap();
    assert!(
        String::from_utf8_lossy(&peek.stdout).contains("ASKED-AND-ANSWERED"),
        "the prompt never reached the session"
    );
}

/// The stall guard measures against the age of the session's last output, not
/// against zero. A session that has been quiet for a while and then answers
/// instantly used to defeat it: the answer was stamped before the Ack for the
/// prompt finished its round trip, so `idle_ms` and the elapsed time grew in
/// lockstep and the guard reported a stall for a prompt that had plainly
/// landed. Anything below the age it started from is output the prompt caused.
///
/// The shell is pinned to `--norc` because the race needs the whole answer —
/// echo, command, new prompt — to finish inside the round trip. A shell that
/// paints a title from its prompt takes longer than that and lands on the safe
/// side of the window, so it never showed the bug.
#[tokio::test]
async fn ask_does_not_cry_stall_when_a_quiet_session_answers_instantly() {
    let daemon = Daemon::start("askquiet");
    assert!(
        daemon
            .cli()
            .args(["new", "quiet", "--cmd", "exec bash --norc -i"])
            .status()
            .unwrap()
            .success()
    );

    // Let the session go properly quiet first: the bug only showed once the
    // last output was older than the round trip that carries the prompt.
    std::thread::sleep(Duration::from_millis(2_500));

    let out = daemon
        .cli()
        .args(["ask", "quiet", "echo INSTANT-ANSWER", "--timeout", "20s"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("no sign of receiving"),
        "the prompt landed, but ask reported a stall: {err}"
    );
    assert!(out.status.success(), "ask failed: {out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "idle");

    let peek = daemon.cli().args(["peek", "quiet"]).output().unwrap();
    assert!(
        String::from_utf8_lossy(&peek.stdout).contains("INSTANT-ANSWER"),
        "the prompt never reached the session"
    );
}

/// A session whose foreground program never reads its input would otherwise
/// absorb the prompt and leave `ask` waiting out the whole timeout. It gives up
/// as soon as it is clear nothing received it — well before the 20s asked for.
///
/// Echo is off deliberately. A cooked-mode tty echoes what is typed into it
/// whether or not the program ever reads it, so echo left on would put output
/// on the wire and the session would look like it had received the prompt.
/// This is the case the guard can actually speak to: nothing came out at all.
#[tokio::test]
async fn ask_gives_up_early_when_nothing_reads_the_prompt() {
    let daemon = Daemon::start("askstall");
    assert!(
        daemon
            .cli()
            .args(["new", "deaf", "--cmd", "sh -c 'stty -echo; exec sleep 300'"])
            .status()
            .unwrap()
            .success()
    );

    let started = std::time::Instant::now();
    let out = daemon
        .cli()
        .args(["ask", "deaf", "anyone there?", "--timeout", "20s"])
        .output()
        .unwrap();
    let took = started.elapsed();

    assert!(!out.status.success(), "ask should have failed: {out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no sign of receiving"),
        "expected a stall report, got: {err}"
    );
    assert!(
        took < Duration::from_secs(15),
        "gave up after {took:?}, which is the timeout rather than the stall guard"
    );
}
