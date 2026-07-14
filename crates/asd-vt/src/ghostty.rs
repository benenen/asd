//! libghostty-vt backend implementation.
//!
//! The libghostty-vt 0.2.x API is unstable; every direct call to it is
//! confined to this file.

use std::cell::RefCell;
use std::rc::Rc;

use libghostty_vt::{
    RenderState, Terminal, TerminalOptions,
    fmt::{Format, Formatter, FormatterOptions},
    key as gkey,
    render::{CellIterator, CursorVisualStyle, Dirty, RowIterator},
    screen::{CellWide, Screen},
    selection::{FormatOptions as SelFormatOptions, Selection},
    style::{RgbColor, Underline},
    terminal::{Point, PointCoordinate, ScrollViewport},
};

use crate::{
    CellSnapshot, CellWidth, CursorShape, CursorSnapshot, Key, KeyEvent, RenderSnapshot, Rgb,
    StyleFlags, UnderlineKind, VtBackend,
};

/// Nominal cell pixel size. Headless usage has no real glyph metrics; used
/// only for the terminal's pixel-size reporting (XTWINOPS, etc.).
const CELL_PX: (u32, u32) = (8, 16);

/// [`VtBackend`] implementation backed by libghostty-vt. `!Send`, owned
/// exclusively by its holding thread.
pub struct GhosttyVt {
    terminal: Terminal<'static, 'static>,
    render_state: RenderState<'static>,
    row_iter: RowIterator<'static>,
    cell_iter: CellIterator<'static>,
    encoder: gkey::Encoder<'static>,
    /// Query replies the terminal produces during `vt_write` (DA/DSR/DECRQM...).
    pty_responses: Rc<RefCell<Vec<u8>>>,
}

impl std::fmt::Debug for GhosttyVt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GhosttyVt").finish_non_exhaustive()
    }
}

impl VtBackend for GhosttyVt {
    fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        assert!(cols > 0 && rows > 0, "terminal dimensions must be non-zero");
        let mut terminal = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: scrollback,
        })
        .expect("libghostty terminal allocation failed");

        let pty_responses = Rc::new(RefCell::new(Vec::new()));
        terminal
            .on_pty_write({
                let buf = Rc::clone(&pty_responses);
                move |_term, data| buf.borrow_mut().extend_from_slice(data)
            })
            .expect("registering pty-write effect failed");

        Self {
            terminal,
            render_state: RenderState::new().expect("render state allocation failed"),
            row_iter: RowIterator::new().expect("row iterator allocation failed"),
            cell_iter: CellIterator::new().expect("cell iterator allocation failed"),
            encoder: gkey::Encoder::new().expect("key encoder allocation failed"),
            pty_responses,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        self.terminal
            .resize(cols, rows, CELL_PX.0, CELL_PX.1)
            .expect("terminal resize failed");
    }

    fn snapshot_vt(&mut self) -> Vec<u8> {
        let opts = FormatterOptions::new()
            .with_format(Format::Vt)
            .with_unwrap(false)
            .with_trim(false)
            // All terminal state required for snapshot fidelity (spec §8 item 2)
            .with_palette(true)
            .with_modes(true)
            .with_scrolling_region(true)
            .with_tabstops(true)
            .with_keyboard(true)
            .with_cursor(true)
            .with_style(true)
            .with_hyperlink(true)
            .with_protection(true)
            .with_kitty_keyboard(true)
            .with_charsets(true);
        let mut formatter =
            Formatter::new(&self.terminal, opts).expect("formatter allocation failed");
        let mut out = formatter
            .format_alloc(None)
            .expect("formatting terminal snapshot failed")
            .to_vec();
        // The Formatter emits the tabstop/scrolling-region sequences after
        // CUP, and those sequences move the cursor (CHA, DECSTBM homing), so
        // an authoritative CUP must be appended at the end.
        let x = self.terminal.cursor_x().unwrap_or(0);
        let y = self.terminal.cursor_y().unwrap_or(0);
        out.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
        out
    }

    fn render_snapshot(&mut self) -> RenderSnapshot {
        let snapshot = self
            .render_state
            .update(&self.terminal)
            .expect("render state update failed");

        let cols = snapshot.cols().expect("render state cols");
        let rows = snapshot.rows().expect("render state rows");
        let colors = snapshot.colors().expect("render state colors");

        let cursor_pos = snapshot
            .cursor_viewport()
            .expect("render state cursor")
            .map(|c| (c.x, c.y));
        let cursor = CursorSnapshot {
            visible: snapshot.cursor_visible().expect("cursor visibility"),
            position: cursor_pos,
            shape: match snapshot.cursor_visual_style().expect("cursor style") {
                CursorVisualStyle::Bar => CursorShape::Bar,
                CursorVisualStyle::Underline => CursorShape::Underline,
                CursorVisualStyle::BlockHollow => CursorShape::BlockHollow,
                _ => CursorShape::Block,
            },
            blinking: snapshot.cursor_blinking().expect("cursor blinking"),
        };

        let mut cells = Vec::with_capacity(rows as usize);
        let mut row_dirty = Vec::with_capacity(rows as usize);
        let mut grapheme_buf = String::new();

        let mut row_iter = self
            .row_iter
            .update(&snapshot)
            .expect("row iterator update failed");
        while let Some(row) = row_iter.next() {
            row_dirty.push(row.dirty().expect("row dirty flag"));
            let mut row_cells = Vec::with_capacity(cols as usize);

            let mut cell_iter = self
                .cell_iter
                .update(row)
                .expect("cell iterator update failed");
            while let Some(cell) = cell_iter.next() {
                grapheme_buf.clear();
                cell.graphemes_utf8(&mut grapheme_buf)
                    .expect("cell grapheme read failed");

                let width = match cell.raw_cell().and_then(|c| c.wide()) {
                    Ok(CellWide::Wide) => CellWidth::Wide,
                    Ok(CellWide::SpacerTail) => CellWidth::SpacerTail,
                    Ok(CellWide::SpacerHead) => CellWidth::SpacerHead,
                    _ => CellWidth::Narrow,
                };

                let flags = if cell.has_styling().unwrap_or(false) {
                    style_flags(cell.style().expect("cell style read failed"))
                } else {
                    StyleFlags::default()
                };

                row_cells.push(CellSnapshot {
                    grapheme: grapheme_buf.clone(),
                    fg: cell.fg_color().expect("cell fg color").map(rgb),
                    bg: cell.bg_color().expect("cell bg color").map(rgb),
                    flags,
                    width,
                });
            }
            // Dirty semantics: this snapshot consumes the row-level dirty flag
            row.set_dirty(false).expect("resetting row dirty failed");
            cells.push(row_cells);
        }
        snapshot
            .set_dirty(Dirty::Clean)
            .expect("resetting global dirty failed");

        RenderSnapshot {
            cols,
            rows,
            cells,
            row_dirty,
            cursor,
            palette: colors.palette.map(rgb),
            foreground: rgb(colors.foreground),
            background: rgb(colors.background),
        }
    }

    fn encode_key(&mut self, ev: KeyEvent) -> Vec<u8> {
        // Modes (DECCKM, kitty flags, modifyOtherKeys...) are synced from the
        // terminal's current state on every call
        self.encoder.set_options_from_terminal(&self.terminal);

        let mut event = gkey::Event::new().expect("key event allocation failed");
        event.set_action(gkey::Action::Press);
        event.set_key(map_key(ev.key));
        event.set_mods(map_mods(ev.mods));

        if let Key::Char(c) = ev.key {
            event.set_unshifted_codepoint(c.to_ascii_lowercase());
        }
        let text = ev.text.or_else(|| match ev.key {
            Key::Char(c) if !c.is_control() => Some(c.to_string()),
            _ => None,
        });
        event.set_utf8(text);

        let mut out = Vec::new();
        self.encoder
            .encode_to_vec(&event, &mut out)
            .expect("key encoding failed");
        out
    }

    fn take_pty_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.pty_responses.borrow_mut())
    }

    fn history_len(&mut self) -> usize {
        let scrollback = self.terminal.scrollback_rows().unwrap_or(0);
        let rows = usize::from(self.cols_rows().1);
        scrollback + rows
    }

    fn fetch_history(&mut self, start: u32, count: u32) -> Vec<Vec<u8>> {
        let total = self.history_len();
        if count == 0 || total == 0 {
            return Vec::new();
        }
        let (cols, _) = self.cols_rows();
        if cols == 0 {
            return Vec::new();
        }
        let start = (start as usize).min(total.saturating_sub(1));
        let end = (start + count as usize).min(total); // exclusive
        let last = end - 1;

        // One read-only pass over the requested window: a screen-space
        // selection from (0, start) to (cols-1, last), formatted as plain
        // text. Screen space spans scrollback + the live screen (row 0 =
        // oldest). Reading per-cell via grid_ref would be O(scrollback) each.
        let x_end = cols - 1;
        let Ok(start_ref) = self.terminal.grid_ref(Point::Screen(PointCoordinate {
            x: 0,
            y: start as u32,
        })) else {
            return Vec::new();
        };
        let Ok(end_ref) = self.terminal.grid_ref(Point::Screen(PointCoordinate {
            x: x_end,
            y: last as u32,
        })) else {
            return Vec::new();
        };
        let selection = Selection::new(start_ref, end_ref, false);
        let opts = SelFormatOptions::new()
            .with_selection(&selection)
            .with_emit_format(Format::Plain)
            .with_unwrap(false)
            .with_trim(true);

        let want = end - start;
        match self.terminal.format_selection_alloc(None, opts) {
            Ok(Some(bytes)) => {
                let mut rows: Vec<Vec<u8>> =
                    bytes.split(|&b| b == b'\n').map(<[u8]>::to_vec).collect();
                // Plain output has no trailing newline, so N rows yield N
                // segments; pad/truncate to exactly the requested count so
                // the client's coordinate math stays exact.
                rows.truncate(want);
                while rows.len() < want {
                    rows.push(Vec::new());
                }
                rows
            }
            // No content selected (e.g. an all-blank window) → blank rows.
            _ => vec![Vec::new(); want],
        }
    }

    fn scrollback_rows(&mut self) -> usize {
        self.terminal.scrollback_rows().unwrap_or(0)
    }

    fn set_scroll(&mut self, lines_up: usize) {
        let max = self.terminal.scrollback_rows().unwrap_or(0);
        let up = lines_up.min(max);
        // Anchor at the bottom, then move up by `up` — deterministic
        // regardless of any auto-follow the last write may have applied.
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
        if up > 0 {
            self.terminal
                .scroll_viewport(ScrollViewport::Delta(-(up as isize)));
        }
    }

    fn is_alt_screen(&mut self) -> bool {
        self.terminal
            .active_screen()
            .map(|s| s == Screen::Alternate)
            .unwrap_or(false)
    }

    fn is_mouse_tracking(&mut self) -> bool {
        self.terminal.is_mouse_tracking().unwrap_or(false)
    }

    fn selection_text(&mut self, sel: crate::Selection) -> String {
        let (cols, rows) = self.cols_rows();
        if cols == 0 || rows == 0 {
            return String::new();
        }
        let clampx = |x: u16| x.min(cols - 1);
        let clampy = |y: u16| y.min(rows - 1);
        let start = PointCoordinate {
            x: clampx(sel.start.0),
            y: u32::from(clampy(sel.start.1)),
        };
        let end = PointCoordinate {
            x: clampx(sel.end.0),
            y: u32::from(clampy(sel.end.1)),
        };
        let (Ok(a), Ok(b)) = (
            self.terminal.grid_ref(Point::Viewport(start)),
            self.terminal.grid_ref(Point::Viewport(end)),
        ) else {
            return String::new();
        };
        let selection = Selection::new(a, b, sel.block);
        let opts = SelFormatOptions::new()
            .with_selection(&selection)
            .with_emit_format(Format::Plain)
            .with_unwrap(true)
            .with_trim(true);
        match self.terminal.format_selection_alloc(None, opts) {
            Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
            _ => String::new(),
        }
    }
}

impl GhosttyVt {
    /// Current (cols, rows) of the terminal.
    fn cols_rows(&self) -> (u16, u16) {
        (
            self.terminal.cols().unwrap_or(0),
            self.terminal.rows().unwrap_or(0),
        )
    }
}

fn rgb(c: RgbColor) -> Rgb {
    Rgb {
        r: c.r,
        g: c.g,
        b: c.b,
    }
}

fn style_flags(s: libghostty_vt::style::Style) -> StyleFlags {
    StyleFlags {
        bold: s.bold,
        italic: s.italic,
        faint: s.faint,
        blink: s.blink,
        inverse: s.inverse,
        invisible: s.invisible,
        strikethrough: s.strikethrough,
        overline: s.overline,
        underline: match s.underline {
            Underline::Single => UnderlineKind::Single,
            Underline::Double => UnderlineKind::Double,
            Underline::Curly => UnderlineKind::Curly,
            Underline::Dotted => UnderlineKind::Dotted,
            Underline::Dashed => UnderlineKind::Dashed,
            _ => UnderlineKind::None,
        },
    }
}

fn map_mods(m: crate::Mods) -> gkey::Mods {
    let mut out = gkey::Mods::empty();
    if m.shift {
        out |= gkey::Mods::SHIFT;
    }
    if m.ctrl {
        out |= gkey::Mods::CTRL;
    }
    if m.alt {
        out |= gkey::Mods::ALT;
    }
    if m.super_key {
        out |= gkey::Mods::SUPER;
    }
    out
}

fn map_key(key: Key) -> gkey::Key {
    use gkey::Key as G;
    match key {
        Key::Enter => G::Enter,
        Key::Escape => G::Escape,
        Key::Backspace => G::Backspace,
        Key::Tab => G::Tab,
        Key::ArrowUp => G::ArrowUp,
        Key::ArrowDown => G::ArrowDown,
        Key::ArrowLeft => G::ArrowLeft,
        Key::ArrowRight => G::ArrowRight,
        Key::Home => G::Home,
        Key::End => G::End,
        Key::PageUp => G::PageUp,
        Key::PageDown => G::PageDown,
        Key::Insert => G::Insert,
        Key::Delete => G::Delete,
        // Numbered consecutively from F1 = 121 through F25
        Key::F(n @ 1..=25) => G::try_from(120 + u32::from(n)).unwrap_or(G::Unidentified),
        Key::F(_) => G::Unidentified,
        Key::Char(c) => match c.to_ascii_lowercase() {
            'a'..='z' => {
                G::try_from(u32::from(G::A) + (c.to_ascii_lowercase() as u32 - 'a' as u32))
                    .unwrap_or(G::Unidentified)
            }
            '0'..='9' => G::try_from(u32::from(G::Digit0) + (c as u32 - '0' as u32))
                .unwrap_or(G::Unidentified),
            ' ' => G::Space,
            '`' => G::Backquote,
            '\\' => G::Backslash,
            '[' => G::BracketLeft,
            ']' => G::BracketRight,
            ',' => G::Comma,
            '=' => G::Equal,
            '-' => G::Minus,
            '.' => G::Period,
            '\'' => G::Quote,
            ';' => G::Semicolon,
            '/' => G::Slash,
            // Everything else (incl. shifted symbols, non-ASCII) goes through
            // the utf8 text path
            _ => G::Unidentified,
        },
    }
}
