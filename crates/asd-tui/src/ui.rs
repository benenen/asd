//! TUI layout and drawing orchestration.
//!
//! Region renderers live in [`side`], [`pane`], and [`bar`]; this module owns
//! shared layout, modal layering, and the stable helpers used by the event loop.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear};

use crate::App;
use crate::modal::{Modal, RenameInput};

mod bar;
mod pane;
mod side;

/// Sidebar width in cells (incl. its 1-cell right border).
pub const SIDEBAR_W: u16 = 28;
/// Column (relative to the sidebar's left) where a row's name/title text
/// starts: col 0 is reserved for the selected row's accent bar, cols 1-2 for
/// the ordinal, so text begins right after at col 3. Shared with the running
/// shimmer so only the text — not the ordinal — is hue-shifted.
pub const ROW_TEXT_X: u16 = 3;

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
    app.cursor_tail = None;
    let (side, pane, bar) = areas(
        f.area(),
        app.sidebar_w,
        app.sidebar_hidden,
        app.status_hidden,
    );
    side::draw(f.buffer_mut(), side, app);
    bar::draw(f.buffer_mut(), bar, app);
    app.process_fx(f.buffer_mut(), side);
    draw_pane(f, app, pane);

    // The git graph overlay covers sidebar and pane both, and suppresses the
    // cursor the same way a modal does. A modal is layered on top of it: the
    // overlay lets asd's leader chord through, so `Ctrl+A r` / `Ctrl+A x` can
    // open one while it is up.
    if app.git_graph.is_some() {
        draw_git_graph(f, app);
    }

    if let Some(modal) = &app.modal
        && let Some(position) = draw_modal(f, modal)
    {
        app.cursor_tail = Some((position.x, position.y, true));
    }
}

fn draw_pane(f: &mut Frame<'_>, app: &mut App, area: Rect) {
    let selection = app.sel_viewport();
    let modal_open = app.modal.is_some();
    let overlay_open = app.git_graph.is_some();
    if let Some(snapshot) = app.snapshot() {
        pane::render(f.buffer_mut(), area, &snapshot, selection);
        // Anchor the OS input-method (IME) popup and TUI programs like codex/vim
        // at the real terminal cursor. Suppress it under a modal or scrollback.
        if !modal_open
            && !overlay_open
            && app.scroll == 0
            && let Some((cx, cy)) = snapshot.cursor.position
            && cx < area.width
            && cy < area.height
        {
            app.cursor_tail = Some((area.x + cx, area.y + cy, snapshot.cursor.visible));
        }
    } else {
        let selectable = app
            .sessions
            .iter()
            .any(|s| app.self_session.as_deref() != Some(&s.name));
        pane::draw_empty(
            f.buffer_mut(),
            area,
            app.view_revoked.as_deref(),
            app.sessions.is_empty(),
            selectable,
            &app.keymap,
        );
    }
}

/// Where the git graph overlay goes: inset from the frame edge so the asd UI
/// stays visible as a border around it, or `None` when the terminal has no room
/// for it at all.
///
/// This runs on `asd ui`'s only thread, so a `Rect` reaching outside the frame
/// would panic the whole client and blank every session the user has open. The
/// inset is therefore dropped rather than saturated on a terminal too small to
/// carry it — a saturating inset produces a zero-width `Rect` at a non-zero `x`,
/// which is both empty and no longer inside the frame — and a rect that still
/// comes out empty yields `None`.
fn overlay_rect(area: Rect) -> Option<Rect> {
    let inset_x = if area.width > 8 { 2 } else { 0 };
    let inset_y = if area.height > 6 { 1 } else { 0 };
    let rect = Rect {
        x: area.x + inset_x,
        y: area.y + inset_y,
        width: area.width.saturating_sub(inset_x * 2),
        height: area.height.saturating_sub(inset_y * 2),
    };
    (rect.width > 0 && rect.height > 0).then_some(rect)
}

/// The git graph overlay. `GitGraph::render` clamps to the buffer as well; the
/// two guards are independent on purpose.
fn draw_git_graph(f: &mut Frame<'_>, app: &mut App) {
    let Some(rect) = overlay_rect(f.area()) else {
        return;
    };
    f.render_widget(Clear, rect);
    if let Some(graph) = app.git_graph.as_mut() {
        f.render_widget(graph, rect);
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

    match modal {
        Modal::KillConfirm { target } => {
            draw_kill_modal(f.buffer_mut(), inner, target);
            None
        }
        Modal::Rename(input) => Some(draw_rename_modal(f.buffer_mut(), inner, input)),
    }
}

fn draw_kill_modal(buf: &mut Buffer, area: Rect, target: &str) {
    let bg = Style::new().bg(MODAL_BG);
    let width = area.width as usize;
    let title = format!(
        "Kill session \"{}\"?",
        truncate(target, width.saturating_sub(16))
    );
    buf.set_string(
        area.x,
        area.y,
        &title,
        bg.fg(TEXT).add_modifier(Modifier::BOLD),
    );
    buf.set_string(
        area.x,
        area.y + 2,
        truncate("[y / Enter] confirm    [n / Esc] cancel", width),
        bg.fg(MUTED),
    );
}

fn draw_rename_modal(buf: &mut Buffer, area: Rect, input: &RenameInput) -> Position {
    let bg = Style::new().bg(MODAL_BG);
    let width = area.width as usize;
    buf.set_string(
        area.x,
        area.y,
        format!(
            "Rename \"{}\"",
            truncate(&input.target, width.saturating_sub(10))
        ),
        bg.fg(TEXT).add_modifier(Modifier::BOLD),
    );
    let field_y = area.y + 1;
    let field = Style::new().bg(SELECT_BG).fg(TEXT);
    for x in area.left()..area.right() {
        if let Some(cell) = buf.cell_mut(Position::new(x, field_y)) {
            cell.set_symbol(" ").set_style(field);
        }
    }
    buf.set_string(
        area.x + 1,
        field_y,
        truncate(&input.text(), width.saturating_sub(2)),
        field,
    );
    let (hint, style) = match &input.error {
        Some(error) => (format!("! {error}"), bg.fg(ALERT)),
        None => ("[Enter] rename    [Esc] cancel".to_string(), bg.fg(MUTED)),
    };
    buf.set_string(area.x, area.y + 3, truncate(&hint, width), style);
    Position::new(
        area.x + 1 + (input.cursor() as u16).min(area.width.saturating_sub(2)),
        field_y,
    )
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
    side::hit(side, sessions, offset, col, row)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The overlay is drawn on `asd ui`'s only thread, so a rect escaping the
    /// frame panics the client and blanks every session at once. Sweep every
    /// small size, including origins that are not 0: an inset that saturates
    /// instead of dropping leaves `x` moved but `width` at 0, which lands
    /// outside the frame.
    #[test]
    fn the_overlay_rect_never_escapes_the_frame() {
        for &(ox, oy) in &[(0u16, 0u16), (3, 2)] {
            for width in 0..=14u16 {
                for height in 0..=12u16 {
                    let area = Rect::new(ox, oy, width, height);
                    let Some(rect) = overlay_rect(area) else {
                        continue;
                    };
                    assert!(rect.width > 0 && rect.height > 0, "{area:?} -> {rect:?}");
                    assert_eq!(
                        rect,
                        rect.intersection(area),
                        "{area:?} -> {rect:?} leaves the frame"
                    );
                }
            }
        }
        assert_eq!(overlay_rect(Rect::new(0, 0, 0, 0)), None);
        assert_eq!(overlay_rect(Rect::new(4, 4, 0, 5)), None);
        assert_eq!(
            overlay_rect(Rect::new(0, 0, 80, 24)),
            Some(Rect::new(2, 1, 76, 22))
        );
    }

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
}
