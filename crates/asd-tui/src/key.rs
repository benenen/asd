//! Map crossterm key events to asd-vt [`KeyEvent`]s (the counterpart of
//! asd-gui's key.rs for iced). Pure and unit-testable; the terminal's own
//! `encode_key` then applies mode state (DECCKM etc.) when producing bytes.

use asd_vt::{Key as VtKey, KeyEvent, Mods};
use ratatui::crossterm::event::{KeyCode, KeyEvent as CtKey, KeyModifiers};

/// Translate a crossterm key press into an asd-vt key event, or `None` for
/// keys the terminal does not encode (bare modifiers, media keys, ...).
pub fn map_key(ev: &CtKey) -> Option<KeyEvent> {
    let mods = Mods {
        shift: ev.modifiers.contains(KeyModifiers::SHIFT),
        ctrl: ev.modifiers.contains(KeyModifiers::CONTROL),
        alt: ev.modifiers.contains(KeyModifiers::ALT),
        super_key: ev.modifiers.contains(KeyModifiers::SUPER),
    };

    let (key, text) = match ev.code {
        KeyCode::Char(c) => (VtKey::Char(c), Some(c.to_string())),
        KeyCode::Enter => (VtKey::Enter, None),
        KeyCode::Esc => (VtKey::Escape, None),
        KeyCode::Backspace => (VtKey::Backspace, None),
        KeyCode::Tab | KeyCode::BackTab => (VtKey::Tab, None),
        KeyCode::Up => (VtKey::ArrowUp, None),
        KeyCode::Down => (VtKey::ArrowDown, None),
        KeyCode::Left => (VtKey::ArrowLeft, None),
        KeyCode::Right => (VtKey::ArrowRight, None),
        KeyCode::Home => (VtKey::Home, None),
        KeyCode::End => (VtKey::End, None),
        KeyCode::PageUp => (VtKey::PageUp, None),
        KeyCode::PageDown => (VtKey::PageDown, None),
        KeyCode::Insert => (VtKey::Insert, None),
        KeyCode::Delete => (VtKey::Delete, None),
        KeyCode::F(n) => (VtKey::F(n), None),
        _ => return None,
    };
    // BackTab is crossterm's Shift+Tab: make the shift explicit so encode_key
    // produces the back-tab sequence.
    let mods = if ev.code == KeyCode::BackTab {
        Mods {
            shift: true,
            ..mods
        }
    } else {
        mods
    };
    Some(KeyEvent { key, mods, text })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEvent as CtKey, KeyEventKind, KeyEventState};

    fn press(code: KeyCode, modifiers: KeyModifiers) -> CtKey {
        CtKey {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn plain_char_carries_text() {
        let ev = map_key(&press(KeyCode::Char('a'), KeyModifiers::NONE)).unwrap();
        assert_eq!(ev.key, VtKey::Char('a'));
        assert_eq!(ev.text.as_deref(), Some("a"));
        assert!(!ev.mods.ctrl);
    }

    #[test]
    fn ctrl_char_keeps_modifier() {
        let ev = map_key(&press(KeyCode::Char('c'), KeyModifiers::CONTROL)).unwrap();
        assert_eq!(ev.key, VtKey::Char('c'));
        assert!(ev.mods.ctrl);
    }

    #[test]
    fn backtab_is_shift_tab() {
        let ev = map_key(&press(KeyCode::BackTab, KeyModifiers::NONE)).unwrap();
        assert_eq!(ev.key, VtKey::Tab);
        assert!(ev.mods.shift);
    }

    #[test]
    fn media_keys_are_ignored() {
        assert!(map_key(&press(KeyCode::CapsLock, KeyModifiers::NONE)).is_none());
    }
}
