//! Plain data types that cross threads (spec §6).
//!
//! [`RenderSnapshot`] and all of its members are **`Send`** — produced by the
//! `!Send` Terminal owned exclusively by its holding thread, then handed over
//! a channel to the GUI/other threads.

/// RGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Cell style bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StyleFlags {
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: UnderlineKind,
}

/// Underline kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnderlineKind {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// Cell width attribute (wide characters such as CJK take two cells).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CellWidth {
    /// Ordinary single-cell character.
    #[default]
    Narrow,
    /// The wide character itself, occupying two cells.
    Wide,
    /// Placeholder for a wide character's second cell; not rendered.
    SpacerTail,
    /// Placeholder left at the end of a soft-wrapped line to make room for a
    /// wide character; not rendered.
    SpacerHead,
}

/// One render cell: grapheme + resolved colors + style bits.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CellSnapshot {
    /// Full grapheme cluster (UTF-8). Empty string means a blank cell or a
    /// wide-character placeholder cell.
    pub grapheme: String,
    /// Resolved foreground color; `None` means use the caller's default
    /// foreground.
    pub fg: Option<Rgb>,
    /// Resolved background color; `None` means use the caller's default
    /// background.
    pub bg: Option<Rgb>,
    pub flags: StyleFlags,
    pub width: CellWidth,
}

/// Cursor shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    Bar,
    #[default]
    Block,
    Underline,
    BlockHollow,
}

/// Cursor state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorSnapshot {
    /// Whether the cursor is visible per terminal modes.
    pub visible: bool,
    /// Viewport coordinates (col, row); `None` when the cursor is outside the
    /// viewport.
    pub position: Option<(u16, u16)>,
    pub shape: CursorShape,
    pub blinking: bool,
}

/// One complete render snapshot frame: plain data, `Send`, for the GUI to
/// draw (spec §6).
///
/// Dirty semantics: changes since the **previous** `render_snapshot()` call;
/// the backend consumes (clears) its internal dirty state when producing a
/// snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderSnapshot {
    pub cols: u16,
    pub rows: u16,
    /// `rows` rows × `cols` cells.
    pub cells: Vec<Vec<CellSnapshot>>,
    /// Per-row dirty flags, length = `rows`.
    pub row_dirty: Vec<bool>,
    pub cursor: CursorSnapshot,
    /// The currently effective 256-color palette.
    pub palette: [Rgb; 256],
    /// Default foreground color.
    pub foreground: Rgb,
    /// Default background color.
    pub background: Rgb,
}

// Contract pinned down: the snapshot crosses threads to iced, so it must be
// fully Send (spec §6).
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<RenderSnapshot>();
};

/// Keyboard key (GUI-framework agnostic; asd-vt must not depend on iced).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Printable character key, after layout resolution.
    Char(char),
    Enter,
    Escape,
    Backspace,
    Tab,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// Function keys F1–F25.
    F(u8),
}

/// Modifier keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub super_key: bool,
}

/// A single key event (press).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    pub mods: Mods,
    /// Text the key produces under the current layout (before Ctrl/Meta
    /// transformation). When `None`, derived from [`Key::Char`].
    pub text: Option<String>,
}

impl KeyEvent {
    /// Key press with no modifiers.
    pub fn plain(key: Key) -> Self {
        Self {
            key,
            mods: Mods::default(),
            text: None,
        }
    }
}

/// Selection (finalized in M1; a placeholder in v0 to keep the trait
/// signature stable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Start (col, row), viewport coordinates.
    pub start: (u16, u16),
    /// End (col, row), inclusive.
    pub end: (u16, u16),
    /// Whether this is a rectangular (block) selection.
    pub block: bool,
}
