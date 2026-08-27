use super::*;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

#[test]
fn defaults_route_and_describe_the_same_bindings() {
    let ctrl_a = press(KeyCode::Char('a'), KeyModifiers::CONTROL);
    let mut map = Keymap::default();

    assert_eq!(map.current_hint().text, "Keybinds: Ctrl+A");
    assert_eq!(map.resolve(&ctrl_a), KeyResolution::Consumed);
    assert!(map.current_hint().prefix_active);
    assert_eq!(
        map.resolve(&press(KeyCode::Down, KeyModifiers::NONE)),
        KeyResolution::Action(KeyAction::SelectNext)
    );
    assert!(!map.current_hint().prefix_active);

    assert_eq!(map.resolve(&ctrl_a), KeyResolution::Consumed);
    assert_eq!(
        map.resolve(&press(KeyCode::Char('1'), KeyModifiers::NONE)),
        KeyResolution::Action(KeyAction::JumpTo(1))
    );
    assert_eq!(map.resolve(&ctrl_a), KeyResolution::Consumed);
    assert_eq!(
        map.resolve(&press(KeyCode::Char('9'), KeyModifiers::NONE)),
        KeyResolution::Action(KeyAction::JumpTo(9))
    );

    assert_eq!(
        map.resolve(&press(KeyCode::PageUp, KeyModifiers::SHIFT)),
        KeyResolution::Action(KeyAction::ScrollPageUp)
    );
    assert_eq!(
        map.resolve(&press(KeyCode::Char('c'), KeyModifiers::NONE)),
        KeyResolution::PassThrough
    );

    assert_eq!(map.resolve(&ctrl_a), KeyResolution::Consumed);
    assert_eq!(
        map.resolve(&ctrl_a),
        KeyResolution::Action(KeyAction::SendLeaderLiteral)
    );
    assert_eq!(map.resolve(&ctrl_a), KeyResolution::Consumed);
    assert_eq!(
        map.resolve(&press(KeyCode::Char('?'), KeyModifiers::NONE)),
        KeyResolution::Action(KeyAction::CancelPrefix)
    );

    let hint = map.current_hint();
    assert_eq!(hint.text, "Keybinds: Ctrl+A");
    assert!(!hint.prefix_active);
    assert_eq!(
        map.invocation_hint(KeyAction::Create),
        Some("Ctrl+A c".to_string())
    );
    assert_eq!(
        map.invocation_hint(KeyAction::Reconnect),
        Some("Ctrl+A R".to_string())
    );

    assert_eq!(map.resolve(&ctrl_a), KeyResolution::Consumed);
    let hint = map.current_hint();
    assert!(hint.prefix_active);
    assert!(hint.text.starts_with("PREFIX  "));
    for expected in [
        "j/↓ next",
        "k/↑ previous",
        "1-9 jump",
        "c new",
        "s status",
        "R reconnect",
        "Esc cancel",
    ] {
        assert!(hint.text.contains(expected), "hint: {}", hint.text);
    }
}

#[test]
fn compiled_overrides_change_routing_and_every_hint_together() {
    let mut map = Keymap::configured(KeymapOverrides {
        leader: Some("Ctrl+B".to_string()),
        prefix: vec![KeyBindingOverride {
            action: KeyAction::Create,
            aliases: vec!["n".to_string()],
        }],
        ..KeymapOverrides::default()
    })
    .unwrap();
    let ctrl_a = press(KeyCode::Char('a'), KeyModifiers::CONTROL);
    let ctrl_b = press(KeyCode::Char('b'), KeyModifiers::CONTROL);

    assert_eq!(map.resolve(&ctrl_a), KeyResolution::PassThrough);
    assert_eq!(map.current_hint().text, "Keybinds: Ctrl+B");
    assert_eq!(map.resolve(&ctrl_b), KeyResolution::Consumed);
    let prefix = map.current_hint().text;
    assert!(prefix.contains("n new"), "hint: {prefix}");
    assert!(!prefix.contains("c new"), "hint: {prefix}");
    assert_eq!(
        map.resolve(&press(KeyCode::Char('n'), KeyModifiers::NONE)),
        KeyResolution::Action(KeyAction::Create)
    );
    assert_eq!(
        map.invocation_hint(KeyAction::Create),
        Some("Ctrl+B n".to_string())
    );

    assert_eq!(map.resolve(&ctrl_b), KeyResolution::Consumed);
    assert_eq!(
        map.resolve(&press(KeyCode::Char('b'), KeyModifiers::NONE)),
        KeyResolution::Action(KeyAction::ToggleSidebar)
    );
}

#[test]
fn jump_bindings_carry_their_ordinal_instead_of_inferring_it_from_the_key() {
    let mut map = Keymap::configured(KeymapOverrides {
        prefix: vec![KeyBindingOverride {
            action: KeyAction::JumpTo(1),
            aliases: vec!["F1".to_string()],
        }],
        ..KeymapOverrides::default()
    })
    .unwrap();

    assert_eq!(
        map.resolve(&press(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        KeyResolution::Consumed
    );
    assert_eq!(
        map.resolve(&press(KeyCode::F(1), KeyModifiers::NONE)),
        KeyResolution::Action(KeyAction::JumpTo(1))
    );
    assert_eq!(
        map.invocation_hint(KeyAction::JumpTo(1)),
        Some("Ctrl+A F1".to_string())
    );
}

#[test]
fn direct_bindings_use_the_same_production_override_path() {
    let mut map = Keymap::configured(KeymapOverrides {
        direct: vec![KeyBindingOverride {
            action: KeyAction::ScrollPageUp,
            aliases: vec!["F2".to_string()],
        }],
        ..KeymapOverrides::default()
    })
    .unwrap();

    assert_eq!(
        map.resolve(&press(KeyCode::F(2), KeyModifiers::NONE)),
        KeyResolution::Action(KeyAction::ScrollPageUp)
    );
    assert_eq!(
        map.resolve(&press(KeyCode::PageUp, KeyModifiers::SHIFT)),
        KeyResolution::PassThrough
    );
}

#[test]
fn modified_ascii_letters_round_trip_through_the_canonical_label() {
    for (configured, code, modifiers, label) in [
        ("Alt+A", KeyCode::Char('a'), KeyModifiers::ALT, "Alt+A"),
        (
            "Super+A",
            KeyCode::Char('a'),
            KeyModifiers::SUPER,
            "Super+A",
        ),
        (
            "Alt+Shift+A",
            KeyCode::Char('A'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
            "Alt+Shift+A",
        ),
    ] {
        let pattern = KeyPattern::parse(configured).unwrap();
        assert_eq!(pattern.label(), label);
        assert!(pattern.matches(&press(code, modifiers)));
        let reparsed = KeyPattern::parse(&pattern.label()).unwrap();
        assert!(reparsed.matches(&press(code, modifiers)));
    }
}

#[test]
fn differently_cased_alt_letters_are_the_same_binding() {
    let errors = Keymap::configured(KeymapOverrides {
        prefix: vec![
            KeyBindingOverride {
                action: KeyAction::Create,
                aliases: vec!["Alt+a".to_string()],
            },
            KeyBindingOverride {
                action: KeyAction::Rename,
                aliases: vec!["Alt+A".to_string()],
            },
        ],
        ..KeymapOverrides::default()
    })
    .unwrap_err();

    assert!(
        errors.0.iter().any(
            |error| matches!(error, KeymapError::ConflictingBinding { key, .. } if key == "Alt+A")
        ),
        "errors: {errors:?}"
    );
}

#[test]
fn compile_rejects_a_prefix_binding_fully_shadowed_by_the_leader() {
    let errors = Keymap::configured(KeymapOverrides {
        leader: Some("Ctrl+B".to_string()),
        prefix: vec![KeyBindingOverride {
            action: KeyAction::Create,
            aliases: vec!["Ctrl+B".to_string()],
        }],
        ..KeymapOverrides::default()
    })
    .unwrap_err();
    assert!(
        errors.0.iter().any(
            |error| matches!(error, KeymapError::LeaderConflict { context: "prefix", key, action: KeyAction::Create } if key == "Ctrl+B")
        ),
        "errors: {errors:?}"
    );
}

#[test]
fn literal_leader_uses_only_the_configured_modifiers() {
    let mut map = Keymap::default();
    let noisy_leader = press(
        KeyCode::Char('a'),
        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
    );

    assert_eq!(map.resolve(&noisy_leader), KeyResolution::Consumed);
    assert_eq!(
        map.resolve(&noisy_leader),
        KeyResolution::Action(KeyAction::SendLeaderLiteral)
    );
    let literal = map.literal_leader();
    assert_eq!(literal.code, KeyCode::Char('a'));
    assert_eq!(literal.modifiers, KeyModifiers::CONTROL);
}

#[test]
fn compile_rejects_conflicting_bindings() {
    let errors = Keymap::configured(KeymapOverrides {
        prefix: vec![
            KeyBindingOverride {
                action: KeyAction::Create,
                aliases: vec!["n".to_string()],
            },
            KeyBindingOverride {
                action: KeyAction::Rename,
                aliases: vec!["n".to_string()],
            },
        ],
        ..KeymapOverrides::default()
    })
    .unwrap_err();
    assert!(
        errors.0.iter().any(
            |error| matches!(error, KeymapError::ConflictingBinding { key, .. } if key == "n")
        ),
        "errors: {errors:?}"
    );
}

#[test]
fn compile_rejects_duplicate_aliases_within_one_binding() {
    let errors = Keymap::configured(KeymapOverrides {
        prefix: vec![KeyBindingOverride {
            action: KeyAction::Create,
            aliases: vec!["n".to_string(), "n".to_string()],
        }],
        ..KeymapOverrides::default()
    })
    .unwrap_err();
    assert!(
        errors.0.iter().any(
            |error| matches!(error, KeymapError::DuplicateAlias { context: "prefix", key, action: KeyAction::Create } if key == "n")
        ),
        "errors: {errors:?}"
    );
}

#[test]
fn the_leader_then_g_opens_the_git_graph() {
    let mut map = Keymap::default();

    assert_eq!(
        map.resolve(&press(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        KeyResolution::Consumed,
        "Ctrl+A arms the prefix"
    );
    assert_eq!(
        map.resolve(&press(KeyCode::Char('g'), KeyModifiers::NONE)),
        KeyResolution::Action(KeyAction::ToggleGitGraph)
    );

    // Without the prefix, `g` still belongs to the session.
    assert_eq!(
        map.resolve(&press(KeyCode::Char('g'), KeyModifiers::NONE)),
        KeyResolution::PassThrough
    );
}

/// The config's names and the actions they stand for are one table read in two
/// directions; a name that only resolves one way is a rebind that reports an
/// action nobody can write down.
#[test]
fn every_config_name_round_trips() {
    for (name, action) in CONFIG_NAMES {
        assert_eq!(KeyAction::from_config_name(name), Some(*action));
        assert_eq!(action.config_name(), *name);
    }
}
