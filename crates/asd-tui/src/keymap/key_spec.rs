//! Parsed key patterns and their matching, display, and overlap semantics.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

use super::KeymapError;

#[derive(Debug, Clone, Copy)]
pub(super) enum ModifierMatch {
    Any,
    Contains(KeyModifiers),
}

#[derive(Debug, Clone)]
pub(super) enum KeyPattern {
    Key {
        code: KeyCode,
        modifiers: ModifierMatch,
    },
    CharRange {
        first: char,
        last: char,
    },
}

impl KeyPattern {
    pub(super) fn code(code: KeyCode) -> Self {
        Self::Key {
            code,
            modifiers: ModifierMatch::Any,
        }
    }

    fn chord(code: KeyCode, required: KeyModifiers) -> Self {
        Self::Key {
            code,
            modifiers: ModifierMatch::Contains(required),
        }
    }

    pub(super) fn matches(&self, key: &KeyEvent) -> bool {
        match self {
            Self::Key { code, modifiers } => {
                key.code == *code
                    && match modifiers {
                        ModifierMatch::Any => true,
                        ModifierMatch::Contains(required) => key.modifiers.contains(*required),
                    }
            }
            Self::CharRange { first, last } => {
                matches!(key.code, KeyCode::Char(c) if (*first..=*last).contains(&c))
            }
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, KeymapError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(KeymapError::InvalidKeySpec {
                value: value.to_string(),
                reason: "key cannot be empty".to_string(),
            });
        }
        let chars = value.chars().collect::<Vec<_>>();
        if chars.len() == 3 && chars[1] == '-' {
            return (chars[0] < chars[2])
                .then_some(Self::CharRange {
                    first: chars[0],
                    last: chars[2],
                })
                .ok_or_else(|| KeymapError::InvalidKeySpec {
                    value: value.to_string(),
                    reason: "range must be ascending".to_string(),
                });
        }

        parse_chord(value)
    }

    pub(super) fn literal_event(&self) -> Option<KeyEvent> {
        let Self::Key { code, modifiers } = self else {
            return None;
        };
        let modifiers = match modifiers {
            ModifierMatch::Any => KeyModifiers::NONE,
            ModifierMatch::Contains(required) => *required,
        };
        Some(KeyEvent {
            code: *code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    pub(super) fn is_fully_shadowed_by(&self, leader: &Self) -> bool {
        code_set_is_subset(self, leader) && modifier_set_is_subset(self, leader)
    }

    pub(super) fn overlaps(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Key { code: left, .. }, Self::Key { code: right, .. }) => left == right,
            (
                Self::CharRange { first, last },
                Self::Key {
                    code: KeyCode::Char(character),
                    ..
                },
            )
            | (
                Self::Key {
                    code: KeyCode::Char(character),
                    ..
                },
                Self::CharRange { first, last },
            ) => (*first..=*last).contains(character),
            (
                Self::CharRange {
                    first: left_first,
                    last: left_last,
                },
                Self::CharRange {
                    first: right_first,
                    last: right_last,
                },
            ) => left_first <= right_last && right_first <= left_last,
            _ => false,
        }
    }

    pub(super) fn label(&self) -> String {
        match self {
            Self::Key { code, modifiers } => {
                let required = match modifiers {
                    ModifierMatch::Any => KeyModifiers::NONE,
                    ModifierMatch::Contains(required) => *required,
                };
                chord_label(code, required)
            }
            Self::CharRange { first, last } => format!("{first}-{last}"),
        }
    }
}

pub(super) fn parse_leader(value: &str) -> Result<KeyPattern, KeymapError> {
    let leader = KeyPattern::parse(value)?;
    if matches!(leader, KeyPattern::CharRange { .. }) {
        return Err(KeymapError::InvalidKeySpec {
            value: value.to_string(),
            reason: "leader must identify one key".to_string(),
        });
    }
    Ok(leader)
}

fn parse_chord(value: &str) -> Result<KeyPattern, KeymapError> {
    let parts = value.split('+').collect::<Vec<_>>();
    let Some(key_name) = parts.last().copied().filter(|name| !name.is_empty()) else {
        return Err(KeymapError::InvalidKeySpec {
            value: value.to_string(),
            reason: "missing key name".to_string(),
        });
    };
    let required = parts[..parts.len() - 1]
        .iter()
        .try_fold(KeyModifiers::NONE, |required, modifier| {
            parse_modifier(value, modifier).map(|parsed| required | parsed)
        })?;
    let code = parse_key_code(key_name, required).ok_or_else(|| KeymapError::InvalidKeySpec {
        value: value.to_string(),
        reason: format!("unknown key '{key_name}'"),
    })?;
    Ok(if required.is_empty() {
        KeyPattern::code(code)
    } else {
        KeyPattern::chord(code, required)
    })
}

fn parse_modifier(value: &str, modifier: &str) -> Result<KeyModifiers, KeymapError> {
    match modifier.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Ok(KeyModifiers::CONTROL),
        "alt" => Ok(KeyModifiers::ALT),
        "shift" => Ok(KeyModifiers::SHIFT),
        "super" | "meta" => Ok(KeyModifiers::SUPER),
        _ => Err(KeymapError::InvalidKeySpec {
            value: value.to_string(),
            reason: format!("unknown modifier '{modifier}'"),
        }),
    }
}

fn code_set_is_subset(alias: &KeyPattern, leader: &KeyPattern) -> bool {
    match (alias, leader) {
        (KeyPattern::Key { code: alias, .. }, KeyPattern::Key { code: leader, .. }) => {
            alias == leader
        }
        (
            KeyPattern::Key {
                code: KeyCode::Char(alias),
                ..
            },
            KeyPattern::CharRange { first, last },
        ) => (*first..=*last).contains(alias),
        (
            KeyPattern::CharRange {
                first: alias_first,
                last: alias_last,
            },
            KeyPattern::CharRange {
                first: leader_first,
                last: leader_last,
            },
        ) => leader_first <= alias_first && alias_last <= leader_last,
        _ => false,
    }
}

fn modifier_set_is_subset(alias: &KeyPattern, leader: &KeyPattern) -> bool {
    let modifiers = |pattern: &KeyPattern| match pattern {
        KeyPattern::Key { modifiers, .. } => *modifiers,
        KeyPattern::CharRange { .. } => ModifierMatch::Any,
    };
    match (modifiers(alias), modifiers(leader)) {
        (_, ModifierMatch::Any) => true,
        (ModifierMatch::Any, ModifierMatch::Contains(_)) => false,
        (ModifierMatch::Contains(alias), ModifierMatch::Contains(leader)) => alias.contains(leader),
    }
}

fn parse_key_code(name: &str, modifiers: KeyModifiers) -> Option<KeyCode> {
    if name.chars().count() == 1 {
        let mut character = name.chars().next()?;
        if !modifiers.is_empty() && character.is_ascii_alphabetic() {
            character = if modifiers.contains(KeyModifiers::SHIFT) {
                character.to_ascii_uppercase()
            } else {
                character.to_ascii_lowercase()
            };
        }
        return Some(KeyCode::Char(character));
    }
    match name.to_ascii_lowercase().as_str() {
        "up" => Some(KeyCode::Up),
        "down" => Some(KeyCode::Down),
        "left" => Some(KeyCode::Left),
        "right" => Some(KeyCode::Right),
        "esc" | "escape" => Some(KeyCode::Esc),
        "enter" => Some(KeyCode::Enter),
        "tab" => Some(KeyCode::Tab),
        "backtab" => Some(KeyCode::BackTab),
        "backspace" => Some(KeyCode::Backspace),
        "pageup" => Some(KeyCode::PageUp),
        "pagedown" => Some(KeyCode::PageDown),
        "home" => Some(KeyCode::Home),
        "end" => Some(KeyCode::End),
        "insert" => Some(KeyCode::Insert),
        "delete" => Some(KeyCode::Delete),
        _ => name
            .strip_prefix('F')
            .or_else(|| name.strip_prefix('f'))
            .and_then(|number| number.parse().ok())
            .map(KeyCode::F),
    }
}

fn chord_label(code: &KeyCode, modifiers: KeyModifiers) -> String {
    let mut parts = Vec::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("Ctrl".to_string());
    }
    if modifiers.contains(KeyModifiers::ALT) {
        parts.push("Alt".to_string());
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("Shift".to_string());
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        parts.push("Super".to_string());
    }
    let key = match code {
        KeyCode::Char(c) if modifiers.is_empty() => c.to_string(),
        KeyCode::Char(c) => c.to_uppercase().collect(),
        KeyCode::Up => "↑".to_string(),
        KeyCode::Down => "↓".to_string(),
        KeyCode::Left => "←".to_string(),
        KeyCode::Right => "→".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "BackTab".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::Insert => "Insert".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    };
    parts.push(key);
    parts.join("+")
}
