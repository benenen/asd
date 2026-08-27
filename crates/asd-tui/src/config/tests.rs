use super::*;
use crate::keymap::KeyResolution;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

const CTRL_ALT: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::ALT);

#[test]
fn switching_sessions_needs_no_prefix() {
    let mut map = Keymap::default();

    assert_eq!(
        map.resolve(&press(KeyCode::Down, CTRL_ALT)),
        KeyResolution::Action(KeyAction::SelectNext)
    );
    assert_eq!(
        map.resolve(&press(KeyCode::Up, CTRL_ALT)),
        KeyResolution::Action(KeyAction::SelectPrevious)
    );

    // The arrows alone still belong to the session, chord or no chord.
    assert_eq!(
        map.resolve(&press(KeyCode::Down, KeyModifiers::NONE)),
        KeyResolution::PassThrough
    );
    // And the prefix route is untouched — the chord is a shortcut, not a move.
    assert_eq!(
        map.resolve(&press(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        KeyResolution::Consumed
    );
    assert_eq!(
        map.resolve(&press(KeyCode::Down, KeyModifiers::NONE)),
        KeyResolution::Action(KeyAction::SelectNext)
    );
}

#[test]
fn a_config_rebinds_the_chord_and_leaves_the_rest_alone() {
    let (mut map, complaint) = from_text(
        r#"
        [session]
        scrollback_lines = 4242

        [keys.direct]
        select_next = ["F8"]
        select_previous = ["F7"]
        "#,
    );
    assert_eq!(complaint, None);

    assert_eq!(
        map.resolve(&press(KeyCode::F(8), KeyModifiers::NONE)),
        KeyResolution::Action(KeyAction::SelectNext)
    );
    assert_eq!(
        map.resolve(&press(KeyCode::F(7), KeyModifiers::NONE)),
        KeyResolution::Action(KeyAction::SelectPrevious)
    );
    // A rebind replaces the default rather than joining it.
    assert_eq!(
        map.resolve(&press(KeyCode::Down, CTRL_ALT)),
        KeyResolution::PassThrough
    );
    // The prefix keeps its own bindings for the same actions.
    assert_eq!(
        map.resolve(&press(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        KeyResolution::Consumed
    );
    assert_eq!(
        map.resolve(&press(KeyCode::Char('j'), KeyModifiers::NONE)),
        KeyResolution::Action(KeyAction::SelectNext)
    );
}

#[test]
fn the_leader_and_the_prefix_table_are_configurable_too() {
    let (mut map, complaint) = from_text(
        r#"
        [keys]
        leader = "Ctrl+B"

        [keys.prefix]
        quit = ["Q"]
        "#,
    );
    assert_eq!(complaint, None);
    assert_eq!(map.current_hint().text, "Keybinds: Ctrl+B");

    assert_eq!(
        map.resolve(&press(KeyCode::Char('b'), KeyModifiers::CONTROL)),
        KeyResolution::Consumed
    );
    assert_eq!(
        map.resolve(&press(KeyCode::Char('Q'), KeyModifiers::SHIFT)),
        KeyResolution::Action(KeyAction::Quit)
    );
}

#[test]
fn the_git_graph_toggle_is_rebindable() {
    let (mut map, complaint) = from_text(
        r#"
[keys.prefix]
toggle_git_graph = ["G"]
"#,
    );
    assert!(complaint.is_none(), "{complaint:?}");

    assert_eq!(
        map.resolve(&press(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        KeyResolution::Consumed
    );
    assert_eq!(
        map.resolve(&press(KeyCode::Char('G'), KeyModifiers::SHIFT)),
        KeyResolution::Action(KeyAction::ToggleGitGraph)
    );

    // The rebind replaces the default rather than adding to it.
    assert_eq!(
        map.resolve(&press(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        KeyResolution::Consumed
    );
    assert_eq!(
        map.resolve(&press(KeyCode::Char('g'), KeyModifiers::NONE)),
        KeyResolution::Action(KeyAction::CancelPrefix)
    );
}

#[test]
fn a_misspelled_action_is_reported_rather_than_ignored() {
    let (mut map, complaint) = from_text(
        r#"
        [keys.direct]
        select_nekst = ["F8"]
        "#,
    );
    assert_eq!(
        complaint.as_deref(),
        Some("config: no such key action keys.direct.select_nekst")
    );
    // What it could not read leaves the default in place.
    assert_eq!(
        map.resolve(&press(KeyCode::Down, CTRL_ALT)),
        KeyResolution::Action(KeyAction::SelectNext)
    );
}

#[test]
fn an_action_the_context_does_not_have_names_itself_in_the_complaint() {
    let (_, complaint) = from_text(
        r#"
        [keys.direct]
        quit = ["F8"]
        "#,
    );
    let complaint = complaint.expect("binding quit outside PREFIX must be reported");
    assert!(
        complaint.contains("quit is not bound in this context"),
        "expected the config's own name for the action, got: {complaint}"
    );
}

#[test]
fn bindings_that_will_not_compile_fall_back_to_every_default() {
    let (mut map, complaint) = from_text(
        r#"
        [keys.direct]
        select_next = ["Ctrl+Alt+Nope"]
        "#,
    );
    assert!(
        complaint.is_some_and(|text| text.contains("default keys")),
        "a rejected keymap has to say it fell back"
    );
    assert_eq!(
        map.resolve(&press(KeyCode::Down, CTRL_ALT)),
        KeyResolution::Action(KeyAction::SelectNext)
    );
}

#[test]
fn an_unparsable_file_still_leaves_a_usable_ui() {
    let (mut map, complaint) = from_text("[keys.direct\nselect_next =");
    assert!(complaint.is_some_and(|text| text.contains("default keys")));
    assert_eq!(
        map.resolve(&press(KeyCode::Up, CTRL_ALT)),
        KeyResolution::Action(KeyAction::SelectPrevious)
    );
}

#[test]
fn a_missing_file_is_not_a_complaint() {
    let (_, complaint) = keymap(Path::new("/nonexistent/asd/does-not-exist.toml"));
    assert_eq!(complaint, None);
}
