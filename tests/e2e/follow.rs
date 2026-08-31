//! `asd follow`: streaming until idle, streaming forever, and the JSONL feed.

use crate::common::*;

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
