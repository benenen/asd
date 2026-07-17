//! Modal overlays: a text-input rename box and a yes/no kill confirmation.
//! Pure state + editing/validation logic (unit-tested here); rendering lives in
//! [`crate::ui`] and key routing in [`crate::App`].

/// An open modal overlay.
pub enum Modal {
    /// Rename `target` to the text being edited.
    Rename(RenameInput),
    /// Confirm killing the session named `target`.
    KillConfirm { target: String },
}

/// A single-line text input. Editing is by **character**, not byte, so multi-
/// byte text (CJK) moves/deletes one glyph at a time and never splits a `char`.
pub struct RenameInput {
    /// The session being renamed (the original name).
    pub target: String,
    chars: Vec<char>,
    /// Cursor position as a char index in `0..=chars.len()`.
    cursor: usize,
    /// Validation error to show under the field, if the last submit failed.
    pub error: Option<String>,
}

impl RenameInput {
    /// Start editing prefilled with `target`, cursor at the end.
    pub fn new(target: String) -> Self {
        let chars: Vec<char> = target.chars().collect();
        let cursor = chars.len();
        Self {
            target,
            chars,
            cursor,
            error: None,
        }
    }

    /// Current edited text.
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// Cursor position, as a char index (for placing the caret).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
        self.error = None;
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
            self.error = None;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
            self.error = None;
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.chars.len();
    }
}

/// Whether a proposed `new` name can be submitted, given the `existing` session
/// names and the `current` (old) name. `Err(message)` is shown in the box.
pub fn validate_rename(new: &str, existing: &[String], current: &str) -> Result<(), String> {
    if new == current {
        return Ok(()); // no-op rename is harmless
    }
    if new.is_empty() {
        return Err("name cannot be empty".into());
    }
    if !asd_proto::paths::is_valid_session_name(new) {
        return Err("use letters, digits, '_' or '-' (max 64)".into());
    }
    if existing.iter().any(|n| n == new) {
        return Err(format!("'{new}' already exists"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_input_edits_by_char() {
        let mut i = RenameInput::new("ab".into());
        assert_eq!(i.text(), "ab");
        assert_eq!(i.cursor(), 2);
        i.insert('c'); // "abc"
        assert_eq!(i.text(), "abc");
        i.left(); // between b and c
        i.backspace(); // remove 'b' → "ac"
        assert_eq!(i.text(), "ac");
        assert_eq!(i.cursor(), 1);
        i.home();
        i.delete(); // remove 'a' → "c"
        assert_eq!(i.text(), "c");
    }

    #[test]
    fn rename_input_is_char_not_byte_indexed() {
        // A multi-byte char must delete as one unit, not panic on a byte split.
        let mut i = RenameInput::new("a中b".into());
        assert_eq!(i.cursor(), 3);
        i.left(); // between 中 and b
        i.backspace(); // removes 中
        assert_eq!(i.text(), "ab");
    }

    #[test]
    fn validate_rejects_empty_invalid_and_duplicate() {
        let existing = vec!["a".to_string(), "b".to_string()];
        assert!(validate_rename("", &existing, "a").is_err()); // empty
        assert!(validate_rename("has space", &existing, "a").is_err()); // invalid char
        assert!(validate_rename("中文", &existing, "a").is_err()); // non-ascii
        assert!(validate_rename("b", &existing, "a").is_err()); // duplicate
        assert!(validate_rename("c", &existing, "a").is_ok()); // fresh + valid
        assert!(validate_rename("a", &existing, "a").is_ok()); // unchanged (no-op)
    }
}
