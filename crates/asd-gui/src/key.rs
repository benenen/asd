//! Map iced keyboard events to asd-vt [`KeyEvent`]s. Pure and unit-testable.

use asd_vt::{Key as VtKey, KeyEvent, Mods};
use iced::keyboard::{Key, Modifiers, key::Named};

/// Translate an iced key press into an asd-vt key event, or `None` for keys
/// the terminal does not encode (bare modifiers, unidentified keys).
pub fn map_key(key: &Key, mods: Modifiers) -> Option<KeyEvent> {
    let m = Mods {
        shift: mods.shift(),
        ctrl: mods.control(),
        alt: mods.alt(),
        super_key: mods.logo(),
    };

    match key {
        Key::Named(named) => {
            let vk = map_named(*named)?;
            let text = match named {
                Named::Space => Some(" ".to_string()),
                _ => None,
            };
            Some(KeyEvent {
                key: vk,
                mods: m,
                text,
            })
        }
        Key::Character(s) => {
            let c = s.chars().next()?;
            Some(KeyEvent {
                key: VtKey::Char(c),
                mods: m,
                text: Some(s.to_string()),
            })
        }
        Key::Unidentified => None,
    }
}

fn map_named(named: Named) -> Option<VtKey> {
    Some(match named {
        Named::Enter => VtKey::Enter,
        Named::Escape => VtKey::Escape,
        Named::Backspace => VtKey::Backspace,
        Named::Tab => VtKey::Tab,
        Named::Space => VtKey::Char(' '),
        Named::ArrowUp => VtKey::ArrowUp,
        Named::ArrowDown => VtKey::ArrowDown,
        Named::ArrowLeft => VtKey::ArrowLeft,
        Named::ArrowRight => VtKey::ArrowRight,
        Named::Home => VtKey::Home,
        Named::End => VtKey::End,
        Named::PageUp => VtKey::PageUp,
        Named::PageDown => VtKey::PageDown,
        Named::Insert => VtKey::Insert,
        Named::Delete => VtKey::Delete,
        Named::F1 => VtKey::F(1),
        Named::F2 => VtKey::F(2),
        Named::F3 => VtKey::F(3),
        Named::F4 => VtKey::F(4),
        Named::F5 => VtKey::F(5),
        Named::F6 => VtKey::F(6),
        Named::F7 => VtKey::F(7),
        Named::F8 => VtKey::F(8),
        Named::F9 => VtKey::F(9),
        Named::F10 => VtKey::F(10),
        Named::F11 => VtKey::F(11),
        Named::F12 => VtKey::F(12),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn character_maps_with_text() {
        let ev = map_key(&Key::Character("a".into()), Modifiers::empty()).unwrap();
        assert_eq!(ev.key, VtKey::Char('a'));
        assert_eq!(ev.text.as_deref(), Some("a"));
        assert!(!ev.mods.ctrl);
    }

    #[test]
    fn ctrl_modifier_is_carried() {
        let ev = map_key(&Key::Character("c".into()), Modifiers::CTRL).unwrap();
        assert_eq!(ev.key, VtKey::Char('c'));
        assert!(ev.mods.ctrl);
    }

    #[test]
    fn named_keys_map() {
        assert_eq!(
            map_key(&Key::Named(Named::Enter), Modifiers::empty())
                .unwrap()
                .key,
            VtKey::Enter
        );
        assert_eq!(
            map_key(&Key::Named(Named::ArrowUp), Modifiers::empty())
                .unwrap()
                .key,
            VtKey::ArrowUp
        );
        assert_eq!(
            map_key(&Key::Named(Named::F5), Modifiers::empty())
                .unwrap()
                .key,
            VtKey::F(5)
        );
    }

    #[test]
    fn unidentified_is_ignored() {
        assert!(map_key(&Key::Unidentified, Modifiers::empty()).is_none());
    }
}
