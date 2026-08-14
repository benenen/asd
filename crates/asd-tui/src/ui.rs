//! Drawing: session sidebar (two-line rows: name + kill mark, command + age)
//! on the left, the live terminal pane on the right, a keybind hint at the
//! bottom of the sidebar — the layout in `images/image.png`.

use asd_vt::{CellWidth, RenderSnapshot, Rgb, UnderlineKind};
use ratatui::Frame;
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear};

use crate::App;
use crate::modal::Modal;

/// Sidebar width in cells (incl. its 1-cell right border).
pub const SIDEBAR_W: u16 = 28;
/// Column (relative to the sidebar's left) where a row's name/title text
/// starts: col 0 is reserved for the selected row's accent bar, cols 1-2 for
/// the ordinal, so text begins right after at col 3. Shared with the running
/// shimmer so only the text — not the ordinal — is hue-shifted.
pub const ROW_TEXT_X: u16 = 3;

/// The session index that the `Ctrl+A <digit>` quick-switch selects, or `None`
/// for a digit with no target. Only `1..=9` map (to the 1st..9th session); the
/// 10th and later have an ordinal but no shortcut.
pub fn jump_index(digit: char) -> Option<usize> {
    ('1'..='9')
        .contains(&digit)
        .then(|| digit as usize - '1' as usize)
}

// Palette (the asd theme, same values as asd-dioxus's app.css).
const TEXT: Color = Color::Rgb(0xE7, 0xE2, 0xD6);
const DIM: Color = Color::Rgb(0x5A, 0x64, 0x72);
const MUTED: Color = Color::Rgb(0x8B, 0x94, 0xA2);
const ACCENT: Color = Color::Rgb(0xF3, 0xB2, 0x4C);
const ALERT: Color = Color::Rgb(0xE5, 0x59, 0x5E);
const OK: Color = Color::Rgb(0x79, 0xD1, 0x8C);
const SELECT_BG: Color = Color::Rgb(0x2E, 0x2A, 0x20);
const RULE: Color = Color::Rgb(0x23, 0x2A, 0x34);
const MODAL_BG: Color = Color::Rgb(0x14, 0x18, 0x20);

/// Narrowest / widest the sidebar may be dragged.
pub const MIN_SIDEBAR: u16 = 12;
pub const MAX_SIDEBAR: u16 = 50;
/// The pane is never squeezed below this many columns by a wide sidebar.
const MIN_PANE: u16 = 20;

/// Split the frame: sidebar | terminal pane, with a full-width keybind/status
/// bar along the bottom. A hidden sidebar gives the pane the full width; a hidden
/// status bar (`status_hidden`) gives the *pane* the full height (the sidebar
/// keeps its height, leaving a blank last row) so a session's input can reach the
/// window's true bottom.
pub fn areas(total: Rect, sidebar_w: u16, hidden: bool, status_hidden: bool) -> (Rect, Rect, Rect) {
    // Rows reserved for the sidebar/pane. The sidebar is always height-1 (its
    // last row is the status bar's neighbor); the pane reclaims that last row
    // only when the status bar is hidden.
    let side_h = total.height.saturating_sub(1);
    let pane_h = if status_hidden { total.height } else { side_h };
    // Keep the pane usable on a narrow terminal: never let the sidebar squeeze
    // it below MIN_PANE, and drop the sidebar entirely when even the sidebar's
    // own minimum plus MIN_PANE won't fit (rather than swallow the pane whole).
    let side_w = if hidden || total.width < MIN_SIDEBAR + MIN_PANE {
        0
    } else {
        sidebar_w.min(total.width - MIN_PANE)
    };
    let side = Rect::new(total.x, total.y, side_w, side_h);
    let pane = Rect::new(
        total.x + side_w,
        total.y,
        total.width.saturating_sub(side_w),
        pane_h,
    );
    // Zero-height (and thus not drawn) when the status bar is hidden.
    let bar = Rect::new(
        total.x,
        total.y + pane_h,
        total.width,
        total.height - pane_h,
    );
    (side, pane, bar)
}

/// The terminal grid the pane offers (what `Attach`/`Resize` request).
pub fn pane_grid(total: Rect, sidebar_w: u16, hidden: bool, status_hidden: bool) -> (u16, u16) {
    let (_, pane, _) = areas(total, sidebar_w, hidden, status_hidden);
    (pane.width.max(1), pane.height.max(1))
}

/// Clamp a desired sidebar width into a usable range for a `total_w`-wide
/// terminal: at least [`MIN_SIDEBAR`], and never so wide the pane drops below
/// [`MIN_PANE`] (capped at [`MAX_SIDEBAR`]).
pub fn clamp_sidebar(desired: i32, total_w: u16) -> u16 {
    let max = MAX_SIDEBAR
        .min(total_w.saturating_sub(MIN_PANE))
        .max(MIN_SIDEBAR);
    desired.clamp(MIN_SIDEBAR as i32, max as i32) as u16
}

/// The column of the draggable divider (the sidebar's right separator), or
/// `None` when the sidebar is hidden (nothing to grab).
pub fn divider_col(sidebar_w: u16, hidden: bool) -> Option<u16> {
    (!hidden).then(|| sidebar_w.saturating_sub(1))
}

/// The sidebar width that puts the divider under mouse column `x`, clamped.
pub fn sidebar_from_drag(x: u16, total_w: u16) -> u16 {
    clamp_sidebar(x as i32 + 1, total_w)
}

/// Move a stored sidebar offset by `delta` sessions and clamp it to the list.
pub fn scroll_sidebar_offset(current: usize, delta: isize, len: usize, cap: usize) -> usize {
    if cap == 0 || len <= cap {
        return 0;
    }
    let max = len - cap;
    current.min(max).saturating_add_signed(delta).min(max)
}

/// Adjust a stored sidebar offset just enough to reveal `active_idx`.
pub fn sidebar_offset_for_selection(
    current: usize,
    active_idx: usize,
    len: usize,
    cap: usize,
) -> usize {
    if cap == 0 || len <= cap {
        return 0;
    }
    let max = len - cap;
    let current = current.min(max);
    let active_idx = active_idx.min(len - 1);
    if active_idx < current {
        active_idx
    } else if active_idx >= current.saturating_add(cap) {
        active_idx + 1 - cap
    } else {
        current
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelTarget {
    Sidebar,
    Pane,
    None,
}

/// Which scrollable region is under a mouse position. The sidebar's rightmost
/// column is its draggable divider, not list content; the status row scrolls
/// neither region.
pub fn wheel_target(
    total: Rect,
    sidebar_w: u16,
    hidden: bool,
    status_hidden: bool,
    col: u16,
    row: u16,
) -> WheelTarget {
    let (side, pane, _) = areas(total, sidebar_w, hidden, status_hidden);
    let in_sidebar = side.width > 1
        && col >= side.left()
        && col < side.right() - 1
        && row >= side.top()
        && row < side.bottom();
    if in_sidebar {
        return WheelTarget::Sidebar;
    }
    let in_pane = pane.width > 0
        && col >= pane.left()
        && col < pane.right()
        && row >= pane.top()
        && row < pane.bottom();
    if in_pane {
        WheelTarget::Pane
    } else {
        WheelTarget::None
    }
}

/// A selection projected into viewport coordinates: `start`..=`end`,
/// row-major, both inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub start: (u16, u16),
    pub end: (u16, u16),
}

pub fn draw(f: &mut Frame<'_>, app: &mut App) {
    // Recomputed every frame by the cursor blocks below; default hidden.
    app.cursor_tail = None;
    let (side, pane, bar) = areas(
        f.area(),
        app.sidebar_w,
        app.sidebar_hidden,
        app.status_hidden,
    );
    draw_sidebar(f.buffer_mut(), side, app);
    draw_bar(f.buffer_mut(), bar, app);
    app.process_fx(f.buffer_mut(), side);

    let sel = app.sel_viewport();
    let modal_open = app.modal.is_some();
    if let Some(snap) = app.snapshot() {
        render_pane(f.buffer_mut(), pane, &snap, sel);
        // Anchor the OS input-method (IME) popup and TUI programs like codex/vim
        // at the pane's cursor — the REAL terminal cursor, not a painted cell
        // (IME and child-TUI focus both anchor to the hardware cursor). The
        // session's own visibility carries over: pi / Claude Code hide theirs
        // and draw their own caret, so the host cursor is moved there but kept
        // hidden — the IME box still floats at the app's input. Emitted as the
        // frame's closing bytes by `FrameBuf::finish`, after the body painted
        // with the cursor hidden. Suppressed under a modal / while scrolled
        // back.
        if !modal_open
            && app.scroll == 0
            && let Some((cx, cy)) = snap.cursor.position
            && cx < pane.width
            && cy < pane.height
        {
            app.cursor_tail = Some((pane.x + cx, pane.y + cy, snap.cursor.visible));
        }
        // The scrollback offset is shown in the status bar (next to the session
        // count), not over the pane's top row — see `draw_bar`.
    } else {
        // Whether any listed session can actually be selected here (the session
        // hosting this UI is shown but not attachable).
        let selectable = app
            .sessions
            .iter()
            .any(|s| app.self_session.as_deref() != Some(&s.name));
        draw_empty_pane(
            f.buffer_mut(),
            pane,
            app.view_revoked.as_deref(),
            app.sessions.is_empty(),
            selectable,
        );
    }

    let caret = match &app.modal {
        Some(modal) => draw_modal(f, modal),
        None => None,
    };
    if let Some(pos) = caret {
        // Rename caret: the REAL cursor, shown, so the IME popup anchors to it
        // while typing (e.g. a CJK session name).
        app.cursor_tail = Some((pos.x, pos.y, true));
    }
}

/// A centered overlay: the rename input box or the kill confirmation. Returns
/// the caret position for the rename input, if one should be shown.
fn draw_modal(f: &mut Frame<'_>, modal: &Modal) -> Option<Position> {
    let area = f.area();
    let w = 54u16.clamp(24, area.width.saturating_sub(4).max(24));
    let h = 6u16.min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 3;
    let m = Rect::new(x, y, w, h);

    f.render_widget(Clear, m);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(ACCENT).bg(MODAL_BG))
        .style(Style::new().bg(MODAL_BG));
    let inner = block.inner(m);
    f.render_widget(block, m);

    let bg = Style::new().bg(MODAL_BG);
    let ix = inner.x;
    let iw = inner.width as usize;
    let buf = f.buffer_mut();
    let mut cursor: Option<Position> = None;
    match modal {
        Modal::KillConfirm { target } => {
            let title = format!(
                "Kill session \"{}\"?",
                truncate(target, iw.saturating_sub(16))
            );
            buf.set_string(
                ix,
                inner.y,
                &title,
                bg.fg(TEXT).add_modifier(Modifier::BOLD),
            );
            buf.set_string(
                ix,
                inner.y + 2,
                truncate("[y / Enter] confirm    [n / Esc] cancel", iw),
                bg.fg(MUTED),
            );
        }
        Modal::Rename(input) => {
            buf.set_string(
                ix,
                inner.y,
                format!(
                    "Rename \"{}\"",
                    truncate(&input.target, iw.saturating_sub(10))
                ),
                bg.fg(TEXT).add_modifier(Modifier::BOLD),
            );
            // Input field on the next line: a leading marker + the edited text.
            let fy = inner.y + 1;
            let text = input.text();
            let field = Style::new().bg(SELECT_BG).fg(TEXT);
            for xx in ix..inner.right() {
                if let Some(c) = buf.cell_mut(Position::new(xx, fy)) {
                    c.set_symbol(" ").set_style(field);
                }
            }
            buf.set_string(ix + 1, fy, truncate(&text, iw.saturating_sub(2)), field);
            // Caret at the char index (exact for the valid ASCII name set).
            let cx = ix + 1 + (input.cursor() as u16).min(inner.width.saturating_sub(2));
            cursor = Some(Position::new(cx, fy));
            // Error, or the key hint.
            let (line, style) = match &input.error {
                Some(e) => (format!("! {e}"), bg.fg(ALERT)),
                None => ("[Enter] rename    [Esc] cancel".to_string(), bg.fg(MUTED)),
            };
            buf.set_string(ix, inner.y + 3, truncate(&line, iw), style);
        }
    }
    cursor
}

fn draw_sidebar(buf: &mut Buffer, area: Rect, app: &App) {
    if area.width < 4 || area.height < 3 {
        return;
    }
    // Right border rule.
    for y in area.top()..area.bottom() {
        buf.set_string(area.right() - 1, y, "│", Style::new().fg(RULE));
    }
    let inner_w = (area.width - 1) as usize;

    // Session rows: two lines each, scrolled so the active row stays visible.
    let offset = app.sidebar_offset();
    let list_bottom = area.bottom();
    let mut y = area.top();
    for (i, s) in app.sessions.iter().enumerate().skip(offset) {
        if y + 1 >= list_bottom {
            break;
        }
        let selected = app.active.as_deref() == Some(&s.name);
        let is_self = app.self_session.as_deref() == Some(&s.name);
        let row_bg = if selected {
            Style::new().bg(SELECT_BG)
        } else {
            Style::new()
        };
        // Paint the row background.
        for line in 0..2 {
            for x in area.left()..area.right() - 1 {
                if let Some(c) = buf.cell_mut(Position::new(x, y + line)) {
                    c.set_style(row_bg);
                }
            }
        }
        let text_x = area.left() + ROW_TEXT_X;
        // Line 1: [ordinal][name] … kill mark. Column 0 is left blank (it only
        // ever carries the selected row's accent bar, drawn below).
        // 1-based ordinal, LEFT-aligned in 2 cols so a single-digit number hugs
        // the frame (col 1) instead of floating a column in; the name stays put
        // at ROW_TEXT_X, so the two are no longer jammed together. Matches the
        // Ctrl+A <n> quick-switch for the first nine rows (dimmer past nine).
        let n = i + 1;
        let num_style = if selected {
            row_bg.fg(ACCENT)
        } else if i < 9 {
            row_bg.fg(MUTED)
        } else {
            row_bg.fg(DIM)
        };
        buf.set_string(area.left() + 1, y, format!("{n:<2}"), num_style);
        // The session hosting this UI is shown but not selectable: dim it. A
        // running session's text is drawn in the saturated accent so the hue
        // shimmer (`App::process_running_fx`) has color to rotate — hue-shifting
        // near-white text would barely change it.
        let name_style = if is_self {
            row_bg.fg(DIM)
        } else if s.running {
            row_bg.fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            row_bg.fg(TEXT).add_modifier(Modifier::BOLD)
        };
        let name = truncate(&s.name, inner_w.saturating_sub(ROW_TEXT_X as usize + 3));
        buf.set_string(text_x, y, &name, name_style);
        buf.set_string(area.right() - 3, y, "x", row_bg.fg(DIM));
        // Line 2: the terminal title (what the session says it's doing),
        // falling back to the foreground command; plus the age, dim.
        let age = short_age(s.created_ms, app.now_ms);
        let cmd_w = inner_w.saturating_sub(ROW_TEXT_X as usize + age.len() + 2);
        let label = if s.title.trim().is_empty() {
            short_cmd(&s.command)
        } else {
            s.title.trim().to_string()
        };
        let cmd = truncate(&label, cmd_w);
        let cmd_fg = if s.running && !is_self { ACCENT } else { MUTED };
        buf.set_string(text_x, y + 1, &cmd, row_bg.fg(cmd_fg));
        buf.set_string(
            area.right() - 2 - age.len() as u16,
            y + 1,
            &age,
            row_bg.fg(DIM),
        );
        // The selected session's row is framed with a vertical bar on each
        // edge (left column and the right rule column).
        if selected {
            for line in 0..2 {
                buf.set_string(area.left(), y + line, "│", row_bg.fg(ACCENT));
                buf.set_string(area.right() - 1, y + line, "│", Style::new().fg(ACCENT));
            }
        }
        // Remember the row for mouse hit-testing.
        let _ = i;
        y += 2;
    }
}

/// Full-width bottom bar: keybind hint on the left, daemon status / notice on
/// the right.
fn draw_bar(buf: &mut Buffer, area: Rect, app: &App) {
    if area.height == 0 || area.width < 8 {
        return;
    }
    let y = area.top();
    // Bar background.
    for x in area.left()..area.right() {
        if let Some(c) = buf.cell_mut(Position::new(x, y)) {
            c.set_style(Style::new().bg(RULE));
        }
    }
    let (hint, style) = if app.prefix {
        (
            "PREFIX  j/k ↑/↓ switch · 1-9 jump · c new · r rename · x kill · b sidebar · R reconnect · q quit · Esc cancel",
            Style::new().fg(ACCENT).bg(RULE),
        )
    } else {
        ("Keybinds: Ctrl+A", Style::new().fg(DIM).bg(RULE))
    };
    buf.set_string(
        area.left() + 1,
        y,
        truncate(hint, area.width.saturating_sub(2) as usize),
        style,
    );

    let (status, sstyle) = if let Some(notice) = &app.notice {
        (notice.clone(), Style::new().fg(ALERT).bg(RULE))
    } else if app.daemon_up {
        // Exclude the session hosting this UI — it can't be attached here, so
        // counting it would overstate what the user can switch to.
        let count = app
            .sessions
            .iter()
            .filter(|s| app.self_session.as_deref() != Some(&s.name))
            .count();
        (
            session_status(count, app.scroll),
            Style::new().fg(OK).bg(RULE),
        )
    } else {
        (
            "daemon down — Ctrl+A R".to_string(),
            Style::new().fg(ALERT).bg(RULE),
        )
    };
    let max = (area.width / 2) as usize;
    let status = truncate(&status, max);
    let x = area.right().saturating_sub(str_width(&status) as u16 + 1);
    // Only draw the status when it clears the hint actually painted on the left
    // (the *truncated* hint width, not the full string).
    let hint_w = str_width(&truncate(hint, area.width.saturating_sub(2) as usize));
    if x > area.left() + 1 + hint_w as u16 {
        buf.set_string(x, y, status, sstyle);
    }
}

/// Which sidebar session row (and whether its kill mark) a click lands on.
/// `offset` is the current scroll offset (see [`sidebar_offset`]) so a click
/// maps to the true session index, not the on-screen row.
pub fn sidebar_hit(
    area: Rect,
    sidebar_w: u16,
    hidden: bool,
    sessions: usize,
    offset: usize,
    col: u16,
    row: u16,
) -> Option<(usize, bool)> {
    let (side, _, _) = areas(area, sidebar_w, hidden, false);
    if side.width == 0 || col >= side.right().saturating_sub(1) || row >= side.bottom() {
        return None;
    }
    let idx = offset + ((row.checked_sub(side.top())?) / 2) as usize;
    if idx >= sessions {
        return None;
    }
    let on_kill = row.saturating_sub(side.top()) % 2 == 0 && col >= side.right().saturating_sub(4);
    Some((idx, on_kill))
}

/// Whether viewport cell `(x, y)` falls inside the row-major selection.
fn in_selection(sel: Option<Selection>, x: u16, y: u16) -> bool {
    let Some(s) = sel else { return false };
    let after_start = y > s.start.1 || (y == s.start.1 && x >= s.start.0);
    let before_end = y < s.end.1 || (y == s.end.1 && x <= s.end.0);
    after_start && before_end
}

/// Paint a [`RenderSnapshot`] into the pane area, reversing the cells covered
/// by the drag selection (the CLI attach client's self-drawn highlight).
fn render_pane(buf: &mut Buffer, area: Rect, snap: &RenderSnapshot, sel: Option<Selection>) {
    let rows = snap.rows.min(area.height);
    let cols = snap.cols.min(area.width);
    for y in 0..area.height {
        for x in 0..area.width {
            let Some(target) = buf.cell_mut(Position::new(area.x + x, area.y + y)) else {
                continue;
            };
            // The pane and the session's grid are not always the same size —
            // hiding the sidebar widens the pane a frame or two before the
            // resize round-trips through the daemon. Blank what the grid does
            // not cover: this function is the only thing that writes the pane,
            // and ratatui emits only changed cells, so anything left here would
            // stay on screen indefinitely.
            let Some(cell) = (y < rows && x < cols).then(|| &snap.cells[y as usize][x as usize])
            else {
                target.reset();
                continue;
            };
            // The cells a wide character reserves render as nothing, but they
            // still have to be *written*: skipping them leaves the previous
            // frame's cell in place for as long as the spacer lasts, and
            // ratatui only emits cells that changed. Blanking also keeps the
            // buffer well-formed for `Buffer::diff`, which assumes no
            // double-width cell is ever followed by a non-blank one.
            if matches!(cell.width, CellWidth::SpacerTail | CellWidth::SpacerHead) {
                target.reset();
                continue;
            }
            let grapheme = cell.host_grapheme();
            if grapheme.is_empty() {
                target.set_symbol(" ");
            } else {
                target.set_symbol(grapheme.as_ref());
            }
            // Ghostty is the terminal model and therefore the authority on how
            // many grid cells this grapheme occupies. Ratatui otherwise
            // recomputes the width with its own Unicode table; the two disagree
            // for some emoji-presentation graphemes (for example, Ghostty says
            // `✔️` is narrow while unicode-width says it is wide). Buffer::diff
            // would then skip the real cell after the grapheme and leave a
            // character from the previous frame on screen.
            let width = match cell.width {
                CellWidth::Narrow => 1,
                CellWidth::Wide => 2,
                CellWidth::SpacerTail | CellWidth::SpacerHead => unreachable!(),
            };
            target.set_diff_option(CellDiffOption::ForcedWidth(
                std::num::NonZeroU16::new(width).unwrap(),
            ));
            let mut style = cell_style(cell);
            // Only cells with written content take the selection highlight —
            // blank (never-written) cells stay plain, so clicking or dragging
            // over empty areas shows no reverse-video block (same rule as the
            // CLI attach client's render).
            if !cell.grapheme.is_empty() && in_selection(sel, x, y) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            target.set_style(style);
        }
        // Clear any pane cells right of the snapshot (after shrink).
        for x in cols..area.width {
            if let Some(t) = buf.cell_mut(Position::new(area.x + x, area.y + y)) {
                t.reset();
            }
        }
    }
}

fn cell_style(cell: &asd_vt::CellSnapshot) -> Style {
    let mut style = Style::new()
        .fg(cell.fg.map(color).unwrap_or(Color::Reset))
        .bg(cell.bg.map(color).unwrap_or(Color::Reset));
    let f = &cell.flags;
    if f.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if f.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if f.faint {
        style = style.add_modifier(Modifier::DIM);
    }
    if f.inverse {
        style = style.add_modifier(Modifier::REVERSED);
    }
    if f.blink {
        style = style.add_modifier(Modifier::SLOW_BLINK);
    }
    if f.invisible {
        style = style.add_modifier(Modifier::HIDDEN);
    }
    if f.strikethrough {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if f.underline != UnderlineKind::None {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

fn color(c: Rgb) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

/// Display width of a single char in terminal cells (CJK/wide = 2, control = 0).
fn ch_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Display width of a string in terminal cells.
fn str_width(s: &str) -> usize {
    s.chars().map(ch_width).sum()
}

/// Truncate `s` to a **display-column** budget `max` (not a char count), so a
/// row's text never overruns the columns after it (e.g. the age box). If it
/// doesn't fit, whole characters are dropped — never splitting a wide (CJK)
/// glyph — and a `…` (1 col) is appended, keeping the result's width `<= max`.
fn truncate(s: &str, max: usize) -> String {
    if str_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    // Reserve one column for the ellipsis; add whole chars while they fit.
    let budget = max - 1;
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = ch_width(c);
        if w + cw > budget {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Compact a session's command for the sidebar (same rules as the GUIs): a
/// bare path shows its basename; anything with arguments is kept whole.
pub fn short_cmd(cmd: &str) -> String {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return String::new();
    }
    if !cmd.contains(char::is_whitespace) && cmd.contains('/') {
        cmd.rsplit('/').next().unwrap_or(cmd).to_string()
    } else {
        cmd.to_string()
    }
}

/// Compact "time since creation": `now`, `5m`, `2h`, `3d`.
pub fn short_age(created_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(created_ms) / 1000;
    if secs < 60 {
        "now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

const ASD_WORDMARK: [&str; 4] = [
    "  __ _ ___  __| |",
    " / _` / __|/ _` |",
    "| (_| \\__ \\ (_| |",
    " \\__,_|___/\\__,_|",
];

/// Clear and paint the pane's empty state. A revoked view keeps the selected
/// session visible in the sidebar while the pane becomes an explicit takeover
/// placard; every other empty state reuses the same wordmark with its own hint.
fn draw_empty_pane(
    buf: &mut Buffer,
    area: Rect,
    revoked: Option<&str>,
    is_empty: bool,
    any_selectable: bool,
) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
                cell.reset();
            }
        }
    }
    if area.width == 0 || area.height == 0 {
        return;
    }

    let mut lines: Vec<(String, Style)> = ASD_WORDMARK
        .iter()
        .map(|line| ((*line).to_string(), Style::new().fg(ACCENT)))
        .collect();
    lines.push((String::new(), Style::new()));
    if let Some(name) = revoked {
        lines.push((
            format!("Session \"{name}\" is open in another asd ui"),
            Style::new().fg(TEXT),
        ));
        lines.push((
            "Select it again to take over".to_string(),
            Style::new().fg(DIM),
        ));
    } else {
        lines.push((
            empty_pane_hint(is_empty, any_selectable).to_string(),
            Style::new().fg(DIM),
        ));
    }

    let visible = lines.len().min(area.height as usize);
    let top = area.y + area.height.saturating_sub(visible as u16) / 2;
    for (offset, (line, style)) in lines.into_iter().take(visible).enumerate() {
        let line = truncate(&line, area.width as usize);
        let x = area.x + area.width.saturating_sub(str_width(&line) as u16) / 2;
        buf.set_string(x, top + offset as u16, line, style);
    }
}

/// The centered pane hint when no session is being viewed. Distinguishes "no
/// sessions at all" from "the only session is this UI's own host" (which `j/k`
/// can't select) — the latter must point at creating one, not at selecting.
fn empty_pane_hint(is_empty: bool, any_selectable: bool) -> &'static str {
    if is_empty {
        "no sessions — Ctrl+A c creates one"
    } else if !any_selectable {
        "only this UI's own session — Ctrl+A c for a new one"
    } else {
        "select a session (Ctrl+A j/k)"
    }
}

/// The right-side status string: the session count, prefixed with the local
/// scrollback offset when scrolled back (shown here, next to the count, rather
/// than over the pane's top row).
fn session_status(count: usize, scroll: usize) -> String {
    if scroll > 0 {
        format!("[+{scroll}] ● {count} sessions")
    } else {
        format!("● {count} sessions")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asd_vt::{CellSnapshot, CursorSnapshot, GhosttyVt, VtBackend};
    use ratatui::backend::{Backend, CrosstermBackend};

    #[test]
    fn truncate_respects_display_width() {
        // Fits → unchanged.
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abc", 3), "abc");
        // ASCII overflow → ellipsis, width within budget.
        let t = truncate("abcdef", 4);
        assert_eq!(t, "abc…");
        assert_eq!(str_width(&t), 4);
        // CJK glyphs are 2 columns each (char count alone would under-measure).
        assert_eq!(str_width("中文"), 4);
        // A wide glyph is never split; result stays within the column budget.
        for max in 1..=8 {
            let t = truncate("中文标题", max);
            assert!(str_width(&t) <= max, "width {} <= {max}", str_width(&t));
        }
        assert!(truncate("中文标题", 5).ends_with('…'));
        assert_eq!(truncate("中", 2), "中"); // exact fit, no ellipsis
        // Degenerate budgets.
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(truncate("abc", 1), "…");
    }

    #[test]
    fn long_title_never_overlaps_the_age_box() {
        // Mirror draw_sidebar's line-2 budget for a 28-wide sidebar.
        let area_w: u16 = 28;
        let age = "13h";
        let inner_w = (area_w - 1) as usize;
        let budget = inner_w.saturating_sub(ROW_TEXT_X as usize + str_width(age) + 2);
        let title = "远端一个非常非常长的会话标题名字啊啊啊啊";
        let shown = truncate(title, budget);
        // The text (starting at ROW_TEXT_X) ends strictly before the age box.
        let text_end = ROW_TEXT_X as usize + str_width(&shown);
        let age_start = (area_w as usize) - 2 - str_width(age);
        assert!(
            text_end < age_start,
            "text_end {text_end} < age_start {age_start}"
        );
        assert!(shown.ends_with('…'));
        assert!(str_width(&shown) <= budget);
    }

    #[test]
    fn narrow_terminal_keeps_a_usable_pane() {
        // Wide: sidebar honored, the pane gets the rest.
        let (side, pane, _) = areas(Rect::new(0, 0, 100, 40), SIDEBAR_W, false, false);
        assert_eq!(side.width, SIDEBAR_W);
        assert_eq!(pane.width, 100 - SIDEBAR_W);
        // Medium-narrow: the sidebar shrinks so the pane never drops below
        // MIN_PANE (28-wide sidebar can't fit alongside a 20-wide pane in 40).
        let (side, pane, _) = areas(Rect::new(0, 0, 40, 40), SIDEBAR_W, false, false);
        assert!(pane.width >= MIN_PANE, "pane {} >= {MIN_PANE}", pane.width);
        assert!(side.width > 0);
        // Too narrow for both: the sidebar drops out; the pane takes it all.
        let w = MIN_SIDEBAR + MIN_PANE - 1;
        let (side, pane, _) = areas(Rect::new(0, 0, w, 40), SIDEBAR_W, false, false);
        assert_eq!(side.width, 0);
        assert_eq!(pane.width, w);
    }

    #[test]
    fn sidebar_wheel_offset_moves_and_clamps_to_the_list() {
        assert_eq!(scroll_sidebar_offset(0, 1, 8, 3), 1);
        assert_eq!(scroll_sidebar_offset(1, -1, 8, 3), 0);
        assert_eq!(scroll_sidebar_offset(0, -1, 8, 3), 0);
        assert_eq!(scroll_sidebar_offset(4, 10, 8, 3), 5);
        assert_eq!(scroll_sidebar_offset(5, 1, 8, 3), 5);
        assert_eq!(scroll_sidebar_offset(5, 1, 2, 3), 0);
        assert_eq!(scroll_sidebar_offset(5, 1, 0, 3), 0);
        assert_eq!(scroll_sidebar_offset(5, 1, 8, 0), 0);
    }

    #[test]
    fn selecting_a_session_keeps_it_inside_the_sidebar_viewport() {
        assert_eq!(sidebar_offset_for_selection(2, 1, 8, 3), 1);
        assert_eq!(sidebar_offset_for_selection(2, 2, 8, 3), 2);
        assert_eq!(sidebar_offset_for_selection(2, 4, 8, 3), 2);
        assert_eq!(sidebar_offset_for_selection(2, 5, 8, 3), 3);
        assert_eq!(sidebar_offset_for_selection(5, 7, 8, 3), 5);
        assert_eq!(sidebar_offset_for_selection(5, 0, 2, 3), 0);
    }

    #[test]
    fn mouse_wheel_target_follows_the_region_under_the_pointer() {
        let total = Rect::new(0, 0, 100, 10);

        assert_eq!(
            wheel_target(total, SIDEBAR_W, false, false, 5, 4),
            WheelTarget::Sidebar
        );
        assert_eq!(
            wheel_target(total, SIDEBAR_W, false, false, SIDEBAR_W, 4),
            WheelTarget::Pane
        );
        assert_eq!(
            wheel_target(total, SIDEBAR_W, false, false, SIDEBAR_W - 1, 4),
            WheelTarget::None
        );
        assert_eq!(
            wheel_target(total, SIDEBAR_W, false, false, 5, 9),
            WheelTarget::None
        );
        assert_eq!(
            wheel_target(total, SIDEBAR_W, true, false, 5, 4),
            WheelTarget::Pane
        );
        assert_eq!(
            wheel_target(Rect::new(0, 0, 30, 10), SIDEBAR_W, false, false, 5, 4),
            WheelTarget::Pane
        );
    }

    #[test]
    fn session_status_prefixes_scroll_offset() {
        assert_eq!(session_status(3, 0), "● 3 sessions");
        assert_eq!(session_status(3, 12), "[+12] ● 3 sessions");
        assert_eq!(session_status(0, 0), "● 0 sessions");
    }

    #[test]
    fn empty_pane_hint_distinguishes_self_only() {
        assert_eq!(
            empty_pane_hint(true, false),
            "no sessions — Ctrl+A c creates one"
        );
        // Sessions exist but none selectable (only the UI's own host): point at
        // creating one, not selecting — j/k can't reach it.
        let self_only = empty_pane_hint(false, false);
        assert!(self_only.contains("Ctrl+A c"));
        assert!(self_only.contains("own session"));
        assert_eq!(
            empty_pane_hint(false, true),
            "select a session (Ctrl+A j/k)"
        );
    }

    #[test]
    fn revoked_view_draws_the_asd_wordmark_and_takeover_hint() {
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        draw_empty_pane(&mut buf, area, Some("review"), false, true);
        let text = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell(Position::new(x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("__ _ ___"), "pane: {text}");
        assert!(
            text.contains("Session \"review\" is open in another asd ui"),
            "pane: {text}"
        );
        assert!(
            text.contains("Select it again to take over"),
            "pane: {text}"
        );
    }

    #[test]
    fn jump_index_maps_1_9_and_ignores_others() {
        assert_eq!(jump_index('1'), Some(0)); // 1 → first session
        assert_eq!(jump_index('9'), Some(8)); // 9 → ninth session
        assert_eq!(jump_index('0'), None);
        assert_eq!(jump_index('a'), None);
        // Only the first nine rows are reachable; the 10th (index 9) and beyond
        // have an ordinal but no digit shortcut.
        assert!(('1'..='9').all(|d| jump_index(d).unwrap() < 9));
    }

    #[test]
    fn short_cmd_basenames_paths_but_keeps_args() {
        assert_eq!(short_cmd("/usr/bin/bash"), "bash");
        assert_eq!(short_cmd("journalctl -f"), "journalctl -f");
        assert_eq!(short_cmd(""), "");
    }

    #[test]
    fn short_age_buckets() {
        let m = 60_000;
        assert_eq!(short_age(0, 30 * 1000), "now");
        assert_eq!(short_age(0, 5 * m), "5m");
        assert_eq!(short_age(0, 120 * m), "2h");
        assert_eq!(short_age(1_000, 0), "now"); // clock skew never panics
    }

    #[test]
    fn pane_grid_reserves_the_sidebar_and_bottom_bar() {
        let (cols, rows) = pane_grid(Rect::new(0, 0, 120, 40), SIDEBAR_W, false, false);
        assert_eq!(cols, 120 - SIDEBAR_W);
        assert_eq!(rows, 39); // one row goes to the full-width keybind bar
        // Hidden: the pane takes the whole width.
        let (cols, _) = pane_grid(Rect::new(0, 0, 120, 40), SIDEBAR_W, true, false);
        assert_eq!(cols, 120);
    }

    #[test]
    fn bar_spans_the_full_width() {
        let (_, _, bar) = areas(Rect::new(0, 0, 120, 40), SIDEBAR_W, false, false);
        assert_eq!(bar, Rect::new(0, 39, 120, 1));
    }

    #[test]
    fn sidebar_hits_map_rows_and_kill_marks() {
        let area = Rect::new(0, 0, 120, 40);
        let hit = |col, row| sidebar_hit(area, SIDEBAR_W, false, 3, 0, col, row);
        // First session row, name area → select.
        assert_eq!(hit(2, 0), Some((0, false)));
        assert_eq!(hit(2, 1), Some((0, false)));
        // Second session row.
        assert_eq!(hit(2, 2), Some((1, false)));
        // Kill mark: first line of a row, near the right edge.
        assert_eq!(hit(SIDEBAR_W - 3, 0), Some((0, true)));
        // Beyond the list or in the pane → no hit.
        assert_eq!(hit(2, 12), None);
        assert_eq!(hit(SIDEBAR_W + 5, 0), None);
        // Hidden sidebar swallows nothing.
        assert_eq!(sidebar_hit(area, SIDEBAR_W, true, 3, 0, 2, 0), None);
        // With a scroll offset, the top on-screen row maps to the true index.
        let scrolled = |col, row| sidebar_hit(area, SIDEBAR_W, false, 30, 5, col, row);
        assert_eq!(scrolled(2, 0), Some((5, false))); // first visible = index 5
        assert_eq!(scrolled(SIDEBAR_W - 3, 2), Some((6, true))); // its kill mark
    }

    #[test]
    fn sidebar_selection_from_the_top_follows_the_active_row() {
        // Everything fits (len <= cap) → no scroll.
        assert_eq!(sidebar_offset_for_selection(0, 0, 5, 10), 0);
        assert_eq!(sidebar_offset_for_selection(0, 4, 5, 10), 0);
        // Active within the first page → still no scroll.
        assert_eq!(sidebar_offset_for_selection(0, 9, 30, 10), 0);
        // Active past the fold → pinned to the viewport bottom.
        assert_eq!(sidebar_offset_for_selection(0, 10, 30, 10), 1);
        assert_eq!(sidebar_offset_for_selection(0, 15, 30, 10), 6);
        // Never scroll past the end.
        assert_eq!(sidebar_offset_for_selection(0, 29, 30, 10), 20);
        // Degenerate: zero-height viewport.
        assert_eq!(sidebar_offset_for_selection(0, 5, 30, 0), 0);
    }

    #[test]
    fn divider_hit_and_drag_width() {
        // The divider sits at the sidebar's last column.
        assert_eq!(divider_col(28, false), Some(27));
        assert_eq!(divider_col(28, true), None);
        // Dragging to column x puts the divider there (width = x + 1), clamped.
        assert_eq!(sidebar_from_drag(20, 120), 21);
        assert_eq!(sidebar_from_drag(3, 120), MIN_SIDEBAR); // too narrow → min
        assert_eq!(sidebar_from_drag(200, 120), MAX_SIDEBAR); // capped
        // A wide sidebar never squeezes the pane below MIN_PANE.
        assert_eq!(clamp_sidebar(45, 50), 50 - MIN_PANE);
    }

    #[test]
    fn selection_highlights_text_but_not_blank_cells() {
        use asd_vt::{CellSnapshot, CursorSnapshot, RenderSnapshot};
        let cell = |g: &str| CellSnapshot {
            grapheme: g.to_string(),
            ..CellSnapshot::default()
        };
        // Row: text, written space, two never-written blanks.
        let snap = RenderSnapshot {
            cols: 4,
            rows: 1,
            cells: vec![std::sync::Arc::new(vec![
                cell("a"),
                cell(" "),
                cell(""),
                cell(""),
            ])],
            row_dirty: vec![true],
            cursor: CursorSnapshot::default(),
            palette: [Rgb::default(); 256],
            foreground: Rgb::default(),
            background: Rgb::default(),
        };
        let area = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(area);
        let sel = Some(Selection {
            start: (0, 0),
            end: (3, 0),
        });
        render_pane(&mut buf, area, &snap, sel);
        let reversed = |x: u16| {
            buf[(x, 0)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        };
        // Text and a written space take the highlight...
        assert!(reversed(0));
        assert!(reversed(1));
        // ...never-written blanks do not (clicking empty areas shows nothing).
        assert!(!reversed(2));
        assert!(!reversed(3));
    }

    /// A snapshot smaller than the pane must not leave the rest of the pane
    /// showing whatever happened to be there before.
    ///
    /// The pane and the session's grid are not always the same size — hiding
    /// the sidebar widens the pane a frame or two before the resize round-trips
    /// through the daemon. Anything painted outside the snapshot's extent
    /// during that window persists indefinitely otherwise: this function never
    /// writes there again, and ratatui only emits cells that changed, so
    /// nothing downstream clears it either.
    #[test]
    fn pane_clears_beyond_a_smaller_snapshot() {
        use asd_vt::{CellSnapshot, CursorSnapshot, RenderSnapshot};
        let cell = |g: &str| CellSnapshot {
            grapheme: g.to_string(),
            ..CellSnapshot::default()
        };
        // Session grid is 2x1; the pane is 6x2.
        let snap = RenderSnapshot {
            cols: 2,
            rows: 1,
            cells: vec![std::sync::Arc::new(vec![cell("a"), cell("b")])],
            row_dirty: vec![true],
            cursor: CursorSnapshot::default(),
            palette: [Rgb::default(); 256],
            foreground: Rgb::default(),
            background: Rgb::default(),
        };
        let area = Rect::new(0, 0, 6, 2);
        // What an untouched cell looks like, to compare the cleared ones with.
        let pristine = Buffer::empty(area)
            .cell(Position::new(0, 0))
            .unwrap()
            .clone();

        let mut buf = Buffer::empty(area);
        // Leftovers from an earlier, larger frame — a coloured block is exactly
        // what a stale cell looks like on screen.
        let stale = [(4u16, 0u16), (1, 1), (5, 1)];
        for (x, y) in stale {
            buf.cell_mut(Position::new(x, y))
                .unwrap()
                .set_symbol("X")
                .set_style(Style::new().bg(Color::Rgb(0, 95, 0)));
        }

        render_pane(&mut buf, area, &snap, None);

        assert_eq!(buf.cell(Position::new(0, 0)).unwrap().symbol(), "a");
        assert_eq!(buf.cell(Position::new(1, 0)).unwrap().symbol(), "b");
        for (x, y) in stale {
            let c = buf.cell(Position::new(x, y)).unwrap();
            assert_eq!(
                (c.symbol(), c.style()),
                (pristine.symbol(), pristine.style()),
                "({x},{y}) outside the snapshot kept its stale content"
            );
        }
    }

    /// The cells a wide character reserves must be written too, not skipped.
    ///
    /// Same trap as `pane_clears_beyond_a_smaller_snapshot`, one layer in: this
    /// function is the pane's only writer and ratatui emits only cells that
    /// changed, so a cell skipped here keeps what was there before for as long
    /// as it stays a spacer. `SpacerHead` — the placeholder a soft wrap leaves
    /// at the end of a line when the next character is wide — is the worse of
    /// the two, since nothing precedes it to cover the stale cell up. Skipping
    /// `SpacerTail` also broke ratatui's documented requirement that a
    /// double-width cell be followed by a blank one (`Buffer::diff`).
    #[test]
    fn pane_clears_wide_char_spacers() {
        use asd_vt::{CellSnapshot, CursorSnapshot, RenderSnapshot};
        let cell = |g: &str, width: CellWidth| CellSnapshot {
            grapheme: g.to_string(),
            width,
            ..CellSnapshot::default()
        };
        // "中" covers cells 0-1; cell 3 is a soft-wrap placeholder.
        let snap = RenderSnapshot {
            cols: 4,
            rows: 1,
            cells: vec![std::sync::Arc::new(vec![
                cell("中", CellWidth::Wide),
                cell("", CellWidth::SpacerTail),
                cell("a", CellWidth::Narrow),
                cell("", CellWidth::SpacerHead),
            ])],
            row_dirty: vec![true],
            cursor: CursorSnapshot::default(),
            palette: [Rgb::default(); 256],
            foreground: Rgb::default(),
            background: Rgb::default(),
        };
        let area = Rect::new(0, 0, 4, 1);
        let pristine = Buffer::empty(area)
            .cell(Position::new(0, 0))
            .unwrap()
            .clone();

        let mut buf = Buffer::empty(area);
        // A green block from an earlier frame, sitting on both spacers.
        let stale = [1u16, 3];
        for x in stale {
            buf.cell_mut(Position::new(x, 0))
                .unwrap()
                .set_symbol("X")
                .set_style(Style::new().bg(Color::Rgb(0, 95, 0)));
        }

        render_pane(&mut buf, area, &snap, None);

        assert_eq!(buf.cell(Position::new(0, 0)).unwrap().symbol(), "中");
        assert_eq!(buf.cell(Position::new(2, 0)).unwrap().symbol(), "a");
        for x in stale {
            let c = buf.cell(Position::new(x, 0)).unwrap();
            assert_eq!(
                (c.symbol(), c.style()),
                (pristine.symbol(), pristine.style()),
                "the spacer at ({x},0) kept its stale content"
            );
        }
    }

    #[test]
    fn pane_diff_uses_the_vt_width_for_emoji_presentation_graphemes() {
        fn cell(grapheme: &str, fg: Rgb) -> CellSnapshot {
            CellSnapshot {
                grapheme: grapheme.to_string(),
                fg: Some(fg),
                width: CellWidth::Narrow,
                ..CellSnapshot::default()
            }
        }

        fn snapshot(cells: Vec<CellSnapshot>) -> RenderSnapshot {
            RenderSnapshot {
                cols: cells.len() as u16,
                rows: 1,
                cells: vec![std::sync::Arc::new(cells)],
                row_dirty: vec![true],
                cursor: CursorSnapshot::default(),
                palette: [Rgb::default(); 256],
                foreground: Rgb::default(),
                background: Rgb::default(),
            }
        }

        fn ansi_diff(previous: &Buffer, next: &Buffer) -> Vec<u8> {
            let writer = crate::FrameBuf::default();
            let mut backend = CrosstermBackend::new(writer.clone());
            backend.draw(previous.diff_iter(next)).unwrap();
            backend.flush().unwrap();
            writer.0.borrow().clone()
        }

        let blue = Rgb {
            r: 20,
            g: 80,
            b: 180,
        };
        let green = Rgb {
            r: 20,
            g: 180,
            b: 80,
        };
        let area = Rect::new(0, 0, 3, 1);

        // Ghostty treats the VS16 grapheme as one cell. Ratatui's Unicode-width
        // table treats it as two. The host output uses VS15 so the real terminal
        // also keeps it narrow, while ForcedWidth keeps ratatui's diff aligned.
        let first = snapshot(vec![
            cell("✔️", blue),
            cell("A", green),
            CellSnapshot::default(),
        ]);
        let second = snapshot(vec![
            cell("✔️", blue),
            cell("B", green),
            CellSnapshot::default(),
        ]);

        let empty = Buffer::empty(area);
        let mut first_buf = Buffer::empty(area);
        render_pane(&mut first_buf, area, &first, None);
        let mut second_buf = Buffer::empty(area);
        render_pane(&mut second_buf, area, &second, None);

        let mut terminal = GhosttyVt::new(3, 1, 0);
        terminal.feed(&ansi_diff(&empty, &first_buf));
        let first_rendered = terminal.render_snapshot();
        assert_eq!(first_rendered.cells[0][1].grapheme, "A");

        terminal.feed(&ansi_diff(&first_buf, &second_buf));
        let rendered = terminal.render_snapshot();

        assert_eq!(rendered.cells[0][0].grapheme, "✔︎");
        assert_eq!(
            rendered.cells[0][1].grapheme, "B",
            "the cell after a VT-narrow VS16 grapheme kept the previous frame's character"
        );
    }

    #[test]
    fn pane_diff_clears_a_line_on_a_host_that_renders_vs16_wide() {
        fn snapshot(cells: Vec<CellSnapshot>) -> RenderSnapshot {
            RenderSnapshot {
                cols: cells.len() as u16,
                rows: 1,
                cells: vec![std::sync::Arc::new(cells)],
                row_dirty: vec![true],
                cursor: CursorSnapshot::default(),
                palette: [Rgb::default(); 256],
                foreground: Rgb::default(),
                background: Rgb::default(),
            }
        }

        fn ansi_diff(previous: &Buffer, next: &Buffer) -> Vec<u8> {
            let writer = crate::FrameBuf::default();
            let mut backend = CrosstermBackend::new(writer.clone());
            backend.draw(previous.diff_iter(next)).unwrap();
            backend.flush().unwrap();
            writer.0.borrow().clone()
        }

        // The captured reproduction contains this exact VS16 warning followed
        // by text ending in `d`. Ghostty assigns the warning one cell, while
        // the user's host terminal renders it as two. Replacing it with a known
        // wide glyph lets Ghostty emulate that host-side width disagreement.
        let suffix = " no covering tests found";
        let mut first_cells = Vec::with_capacity(suffix.chars().count() + 2);
        first_cells.push(CellSnapshot {
            grapheme: "⚠️".to_string(),
            width: CellWidth::Narrow,
            ..CellSnapshot::default()
        });
        first_cells.extend(suffix.chars().map(|character| CellSnapshot {
            grapheme: character.to_string(),
            width: CellWidth::Narrow,
            ..CellSnapshot::default()
        }));
        // The trailing blank is unchanged in the next frame, so ratatui will
        // not emit it. A host-side width drift leaves the old final `d` there.
        first_cells.push(CellSnapshot::default());
        let width = first_cells.len() as u16;
        let area = Rect::new(0, 0, width, 1);

        let mut first_buf = Buffer::empty(area);
        render_pane(&mut first_buf, area, &snapshot(first_cells), None);
        let mut previous_cells = vec![
            CellSnapshot {
                grapheme: "X".to_string(),
                width: CellWidth::Narrow,
                ..CellSnapshot::default()
            };
            width as usize - 1
        ];
        previous_cells.push(CellSnapshot::default());
        let mut previous_buf = Buffer::empty(area);
        render_pane(&mut previous_buf, area, &snapshot(previous_cells), None);
        let mut blank_buf = Buffer::empty(area);
        render_pane(
            &mut blank_buf,
            area,
            &snapshot(vec![CellSnapshot::default(); width as usize]),
            None,
        );

        let emulate_wide_vs16 = |bytes: Vec<u8>| {
            String::from_utf8(bytes)
                .unwrap()
                .replace("⚠️", "中")
                .into_bytes()
        };
        let mut host = GhosttyVt::new(width, 1, 0);
        host.feed(&ansi_diff(&blank_buf, &previous_buf));
        let first_diff = ansi_diff(&previous_buf, &first_buf);
        let blank_diff = ansi_diff(&first_buf, &blank_buf);
        host.feed(&emulate_wide_vs16(first_diff));
        host.feed(&emulate_wide_vs16(blank_diff));

        let rendered = host.render_snapshot();
        assert!(
            rendered.cells[0]
                .iter()
                .all(|cell| cell.grapheme.trim().is_empty()),
            "clearing the shorter line left old text on the host: {:?}",
            rendered.cells[0]
        );
    }

    #[test]
    fn pane_leaves_unstyled_cells_on_the_host_terminal_colors() {
        let foreground = Rgb {
            r: 240,
            g: 241,
            b: 242,
        };
        let background = Rgb {
            r: 10,
            g: 11,
            b: 12,
        };
        let snap = RenderSnapshot {
            cols: 1,
            rows: 1,
            cells: vec![std::sync::Arc::new(vec![CellSnapshot {
                grapheme: "x".to_string(),
                width: CellWidth::Narrow,
                ..CellSnapshot::default()
            }])],
            row_dirty: vec![true],
            cursor: CursorSnapshot::default(),
            palette: [Rgb::default(); 256],
            foreground,
            background,
        };
        let area = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(area);

        render_pane(&mut buf, area, &snap, None);

        let cell = &buf[(0, 0)];
        assert_eq!(cell.fg, Color::Reset);
        assert_eq!(cell.bg, Color::Reset);
    }
}
