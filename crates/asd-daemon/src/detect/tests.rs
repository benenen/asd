//! Detection is exercised against captured screens rather than against
//! hand-built strings: a rule is only worth what it says about a screen the
//! agent actually draws, and a fixture is the only thing that keeps that
//! honest when the rules are edited later.
//!
//! Each fixture's first line is `#title: <text>` — the terminal title that
//! came with the screen, which several rules read.

use super::*;

/// The shipped fixtures. Adding a screen here (and a case below) is how a new
/// rule earns its place.
const FIXTURES: &[(&str, &str)] = &[
    ("claude-idle", include_str!("fixtures/claude-idle.txt")),
    (
        "claude-idle-stock",
        include_str!("fixtures/claude-idle-stock.txt"),
    ),
    (
        "claude-working",
        include_str!("fixtures/claude-working.txt"),
    ),
    (
        "claude-working-no-title",
        include_str!("fixtures/claude-working-no-title.txt"),
    ),
    (
        "claude-blocked-permission",
        include_str!("fixtures/claude-blocked-permission.txt"),
    ),
    (
        "claude-blocked-form",
        include_str!("fixtures/claude-blocked-form.txt"),
    ),
    (
        "claude-transcript-viewer",
        include_str!("fixtures/claude-transcript-viewer.txt"),
    ),
    ("codex-working", include_str!("fixtures/codex-working.txt")),
    ("codex-idle", include_str!("fixtures/codex-idle.txt")),
    ("shell", include_str!("fixtures/shell.txt")),
];

/// Split a fixture into its title and its screen lines.
fn fixture(name: &str) -> (String, Vec<String>) {
    let text = FIXTURES
        .iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("no fixture named {name}"))
        .1;
    let mut lines = text.lines();
    let title = lines
        .next()
        .and_then(|l| l.strip_prefix("#title:"))
        .unwrap_or_else(|| panic!("fixture {name} must start with a #title: line"))
        .trim()
        .to_string();
    (title, lines.map(str::to_string).collect())
}

/// Detect against a fixture, returning the state and the rule that produced it.
fn detect(command: &str, name: &str) -> (AgentState, Option<String>) {
    let (title, lines) = fixture(name);
    let screen = Screen {
        title: &title,
        lines: &lines,
    };
    let detector = Detector::load(None);
    let rule = detector
        .matching_rule(command, &screen)
        .map(|r| r.id.clone());
    (detector.detect(command, &screen), rule)
}

#[test]
fn an_idle_prompt_reads_as_idle() {
    // Captured from a live session: a user statusline where the stock build
    // prints "? for shortcuts", so the prompt cursor is what carries the state.
    assert_eq!(
        detect("claude", "claude-idle"),
        (AgentState::Idle, Some("prompt_box_ready".to_string()))
    );
    // The stock build's hint line still works.
    assert_eq!(
        detect("claude", "claude-idle-stock"),
        (AgentState::Idle, Some("prompt_box_ready".to_string()))
    );
}

#[test]
fn a_running_turn_reads_as_working() {
    assert_eq!(
        detect("claude", "claude-working"),
        (AgentState::Working, Some("title_spinner".to_string()))
    );
}

#[test]
fn a_running_turn_is_caught_without_the_title_spinner() {
    // The same screen with an unmarked title. It carries an idle-looking
    // prompt and no "esc to interrupt" — the build captured from shows
    // neither — so the spinner line is the only thing left saying it is busy.
    // Getting this wrong reports a working agent as ready for input.
    assert_eq!(
        detect("claude", "claude-working-no-title"),
        (AgentState::Working, Some("spinner_line".to_string()))
    );
}

#[test]
fn a_permission_dialog_reads_as_blocked() {
    // The title says at-rest here; a dialog is exactly the case where "not
    // producing output" and "needs a person" have to come apart.
    assert_eq!(
        detect("claude", "claude-blocked-permission"),
        (AgentState::Blocked, Some("permission_prompt".to_string()))
    );
}

#[test]
fn a_selection_form_reads_as_blocked() {
    assert_eq!(
        detect("claude", "claude-blocked-form"),
        (AgentState::Blocked, Some("confirmation_form".to_string()))
    );
}

#[test]
fn a_replayed_transcript_is_declined_rather_than_guessed() {
    // The screen carries a permission dialog, but it is a recording of one.
    // Answering `unknown` is the correct claim; answering `blocked` would put
    // a session on the list as needing attention it does not need.
    assert_eq!(
        detect("claude", "claude-transcript-viewer"),
        (AgentState::Unknown, Some("transcript_viewer".to_string()))
    );
}

#[test]
fn codex_is_recognized_by_its_own_manifest() {
    assert_eq!(
        detect("codex", "codex-working"),
        (AgentState::Working, Some("turn_in_progress".to_string()))
    );
    // Captured from a live session, reached through the node interpreter the
    // way the daemon will see it.
    assert_eq!(
        detect(
            "node /root/.nvm/versions/node/v24.16.0/bin/codex",
            "codex-idle"
        ),
        (AgentState::Idle, Some("composer_ready".to_string()))
    );
}

#[test]
fn a_plain_shell_claims_nothing() {
    // No manifest for the command, so no rules run at all.
    assert_eq!(detect("bash", "shell"), (AgentState::Unknown, None));
}

#[test]
fn an_agents_rules_do_not_run_against_another_agents_screen() {
    // Claude's rules must not be applied to a shell that happens to print
    // something familiar, and vice versa.
    assert_eq!(detect("claude", "shell").0, AgentState::Unknown);
    assert_eq!(detect("codex", "claude-idle").0, AgentState::Unknown);
}

#[test]
fn the_agent_is_read_out_of_the_command() {
    assert_eq!(agent_id("claude").as_deref(), Some("claude"));
    assert_eq!(
        agent_id("/opt/bin/claude --resume").as_deref(),
        Some("claude")
    );
    assert_eq!(
        agent_id("C:\\tools\\Claude.exe --resume").as_deref(),
        Some("claude")
    );
    assert_eq!(agent_id("").as_deref(), None);
    assert_eq!(agent_id("   ").as_deref(), None);
}

#[test]
fn an_interpreter_is_looked_past_to_the_script_it_runs() {
    // Codex is installed as a node script, so the foreground command really
    // does read `node .../bin/codex` — taking the first word would leave every
    // Codex session unrecognized.
    assert_eq!(
        agent_id("node /root/.nvm/versions/node/v24.16.0/bin/codex").as_deref(),
        Some("codex")
    );
    assert_eq!(
        agent_id("node --enable-source-maps /usr/lib/node_modules/codex").as_deref(),
        Some("codex")
    );
    // Flags are stepped over on the way, so `-m <module>` names the module.
    assert_eq!(
        agent_id("python3 -m some_agent").as_deref(),
        Some("some_agent")
    );
    // An interpreter with nothing after it names no agent.
    assert_eq!(agent_id("node").as_deref(), None);
    assert_eq!(agent_id("node --version").as_deref(), None);
}

#[test]
fn every_embedded_manifest_parses() {
    // `Detector::load` warns and skips a broken manifest so the daemon keeps
    // serving; that is the right runtime behavior and the wrong test result.
    for text in EMBEDDED {
        let manifest: Manifest = toml::from_str(text).expect("embedded manifest must parse");
        assert!(!manifest.rules.is_empty(), "{} has no rules", manifest.id);
        assert!(manifest.min_engine_version <= ENGINE_VERSION);
    }
    assert_eq!(Detector::load(None).manifests.len(), EMBEDDED.len());
}

#[test]
fn a_rule_with_no_conditions_never_fires() {
    // The guard that matters most: unknown keys are ignored for forward
    // compatibility, so a misspelled predicate leaves a rule with nothing to
    // test. It must match nothing rather than everything.
    let manifest: Manifest = toml::from_str(
        r#"
        id = "ghost"
        [[rules]]
        id = "empty"
        state = "working"
        priority = 9999
        region = "whole_screen"
        contain5 = ["typo"]
        "#,
    )
    .unwrap();
    let detector = Detector {
        manifests: vec![manifest],
    };
    let lines = vec!["anything at all".to_string()];
    let screen = Screen {
        title: "",
        lines: &lines,
    };

    assert_eq!(detector.detect("ghost", &screen), AgentState::Unknown);
}

#[test]
fn a_line_predicate_with_no_conditions_never_fires() {
    let manifest: Manifest = toml::from_str(
        r#"
        id = "ghost"
        [[rules]]
        id = "empty-line"
        state = "working"
        priority = 9999
        region = "whole_screen"
        line = [{ contain5 = ["typo"] }]
        "#,
    )
    .unwrap();
    let detector = Detector {
        manifests: vec![manifest],
    };
    let lines = vec!["anything at all".to_string()];
    let screen = Screen {
        title: "",
        lines: &lines,
    };

    assert_eq!(detector.detect("ghost", &screen), AgentState::Unknown);
}

#[test]
fn a_line_predicates_conditions_must_hold_on_one_line() {
    // The reason `line` exists: two markers on two unrelated lines must not
    // satisfy a rule that means "a line with both".
    let manifest: Manifest = toml::from_str(
        r#"
        id = "ghost"
        [[rules]]
        id = "same-line"
        state = "working"
        region = "whole_screen"
        line = [{ first_char_in = ["00b7"], contains = ["… ("] }]
        "#,
    )
    .unwrap();
    let detector = Detector {
        manifests: vec![manifest],
    };

    let split = vec!["· a bullet".to_string(), "elapsed… (9s)".to_string()];
    assert_eq!(
        detector.detect(
            "ghost",
            &Screen {
                title: "",
                lines: &split
            }
        ),
        AgentState::Unknown
    );

    let together = vec!["· Puttering… (9m 23s · ↓ 34.3k tokens)".to_string()];
    assert_eq!(
        detector.detect(
            "ghost",
            &Screen {
                title: "",
                lines: &together
            }
        ),
        AgentState::Working
    );
}

#[test]
fn a_manifest_needing_a_newer_engine_is_skipped() {
    let dir = temp_dir("engine");
    std::fs::write(
        dir.join("claude.toml"),
        r#"
        id = "claude"
        min_engine_version = 99
        [[rules]]
        id = "always"
        state = "working"
        region = "whole_screen"
        contains = ["x"]
        "#,
    )
    .unwrap();

    // The override replaces the embedded claude manifest, then loses to the
    // engine check — so claude has no rules at all rather than the built-in
    // ones plus a file the engine cannot fully read.
    let detector = Detector::load(Some(&dir));
    assert!(!detector.manifests.iter().any(|m| m.id == "claude"));
    assert!(detector.manifests.iter().any(|m| m.id == "codex"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_user_manifest_replaces_the_embedded_one_of_the_same_id() {
    let dir = temp_dir("override");
    std::fs::write(
        dir.join("claude.toml"),
        r#"
        id = "claude"
        [[rules]]
        id = "mine"
        state = "blocked"
        priority = 10
        region = "whole_screen"
        line = [{ starts_with = ["❯"] }]
        "#,
    )
    .unwrap();

    let (title, lines) = fixture("claude-idle");
    let screen = Screen {
        title: &title,
        lines: &lines,
    };
    let detector = Detector::load(Some(&dir));

    // Replaced, not merged: the built-in idle rule is gone, so the user's
    // reading of that same screen is the only one left.
    assert_eq!(detector.detect("claude", &screen), AgentState::Blocked);
    assert_eq!(
        detector
            .matching_rule("claude", &screen)
            .map(|r| r.id.as_str()),
        Some("mine")
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_unparsable_user_manifest_leaves_the_built_in_alone() {
    let dir = temp_dir("broken");
    std::fs::write(dir.join("claude.toml"), "id = \"claude\"\nrules = 7\n").unwrap();

    let (title, lines) = fixture("claude-idle");
    let screen = Screen {
        title: &title,
        lines: &lines,
    };

    assert_eq!(
        Detector::load(Some(&dir)).detect("claude", &screen),
        AgentState::Idle
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_missing_override_directory_is_not_an_error() {
    let detector = Detector::load(Some(std::path::Path::new("/nonexistent/asd/agents")));
    assert_eq!(detector.manifests.len(), EMBEDDED.len());
}

#[test]
fn regions_parse_and_reject() {
    use super::manifest::Region;

    let parse = |s: &str| Region::try_from(s.to_string());
    assert_eq!(parse("osc_title"), Ok(Region::OscTitle));
    assert_eq!(parse("whole_screen"), Ok(Region::WholeScreen));
    assert_eq!(
        parse("bottom_non_empty_lines(12)"),
        Ok(Region::BottomNonEmptyLines(12))
    );
    // A region that can never match is a mistake worth reporting at load.
    assert!(parse("bottom_non_empty_lines(0)").is_err());
    assert!(parse("bottom_non_empty_lines(x)").is_err());
    assert!(parse("prompt_box_body").is_err());
}

#[test]
fn bottom_non_empty_lines_skips_the_blanks_it_counts_past() {
    // An agent's prompt box floats above a variable run of blank rows; a
    // region counted in raw rows would fall off the bottom of the screen.
    let lines: Vec<String> = ["one", "", "two", "", "", ""]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let screen = Screen {
        title: "t",
        lines: &lines,
    };

    assert_eq!(
        screen.region(Region::BottomNonEmptyLines(2)),
        vec!["one".to_string(), "two".to_string()]
    );
    // Fewer lines than asked for is all of them, in screen order.
    assert_eq!(
        screen.region(Region::BottomNonEmptyLines(9)),
        vec!["one".to_string(), "two".to_string()]
    );
    assert_eq!(screen.region(Region::OscTitle), vec!["t".to_string()]);
}

/// A fresh directory under the system temp dir, named for the case.
fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "asd-detect-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run the rules against a screen captured from a live session. This is how a
/// manifest gets widened from a real capture rather than from memory:
///
/// ```text
/// asd inspect NAME --json | jq -r '"#title: " + .title'  > /tmp/cap.txt
/// asd peek NAME                                         >> /tmp/cap.txt
/// ASD_DETECT_CAPTURE=/tmp/cap.txt ASD_DETECT_COMMAND='claude' \
///   cargo test -p asd-daemon captured_screen -- --ignored --nocapture
/// ```
///
/// Ignored by default because it needs that file, and because a capture is a
/// screenful of whatever the session was doing — useful to look at, not
/// something to check in.
#[test]
#[ignore = "needs a capture in $ASD_DETECT_CAPTURE"]
fn captured_screen() {
    let path = std::env::var("ASD_DETECT_CAPTURE").expect("set ASD_DETECT_CAPTURE");
    let command = std::env::var("ASD_DETECT_COMMAND").expect("set ASD_DETECT_COMMAND");
    let text = std::fs::read_to_string(&path).expect("reading the capture");

    let mut lines = text.lines();
    let title = lines
        .next()
        .and_then(|l| l.strip_prefix("#title:"))
        .expect("the capture must start with a #title: line")
        .trim()
        .to_string();
    let lines: Vec<String> = lines.map(str::to_string).collect();
    let screen = Screen {
        title: &title,
        lines: &lines,
    };

    let detector = Detector::load(None);
    println!(
        "{path}: agent={:?} state={:?} rule={:?}",
        agent_id(&command),
        detector.detect(&command, &screen),
        detector.matching_rule(&command, &screen).map(|r| &r.id),
    );
}
