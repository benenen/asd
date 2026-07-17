//! Drawing: session sidebar (two-line rows: name + kill mark, command + age)
//! on the left, the live terminal pane on the right, a keybind hint at the
//! bottom of the sidebar — the layout in `images/image.png`.

use asd_vt::{CellWidth, RenderSnapshot, Rgb, UnderlineKind};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear};

use crate::App;
use crate::modal::Modal;

/// Sidebar width in cells (incl. its 1-cell right border).
pub const SIDEBAR_W: u16 = 28;
/// Column (relative to the sidebar's left) where a row's name/title text
/// starts, after the 1-col marker and the 2-col ordinal. Shared with the
/// running-shimmer so only the text — not the ordinal — is hue-shifted.
pub const ROW_TEXT_X: u16 = 4;

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

/// Split the frame: sidebar | terminal pane, with a full-width keybind/status
/// bar along the bottom.
pub fn areas(total: Rect) -> (Rect, Rect, Rect) {
    let body_h = total.height.saturating_sub(1);
    let side_w = SIDEBAR_W.min(total.width);
    let side = Rect::new(total.x, total.y, side_w, body_h);
    let pane = Rect::new(
        total.x + side_w,
        total.y,
        total.width.saturating_sub(side_w),
        body_h,
    );
    let bar = Rect::new(
        total.x,
        total.y + body_h,
        total.width,
        total.height - body_h,
    );
    (side, pane, bar)
}

/// The terminal grid the pane offers (what `Attach`/`Resize` request).
pub fn pane_grid(total: Rect) -> (u16, u16) {
    let (_, pane, _) = areas(total);
    (pane.width.max(1), pane.height.max(1))
}

/// A selection projected into viewport coordinates: `start`..=`end`,
/// row-major, both inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub start: (u16, u16),
    pub end: (u16, u16),
}

pub fn draw(f: &mut Frame<'_>, app: &mut App) {
    let (side, pane, bar) = areas(f.area());
    draw_sidebar(f.buffer_mut(), side, app);
    draw_bar(f.buffer_mut(), bar, app);
    app.process_fx(f.buffer_mut(), side);

    let sel = app.sel_viewport();
    let modal_open = app.modal.is_some();
    if let Some(snap) = app.snapshot() {
        render_pane(f.buffer_mut(), pane, &snap, sel);
        // A real terminal cursor when following live output — suppressed under a
        // modal (the modal owns the cursor).
        if !modal_open
            && app.scroll == 0
            && snap.cursor.visible
            && let Some((cx, cy)) = snap.cursor.position
            && cx < pane.width
            && cy < pane.height
        {
            f.set_cursor_position(Position::new(pane.x + cx, pane.y + cy));
        }
        if app.scroll > 0 {
            let tag = format!("[+{}] ", app.scroll);
            let x = pane.right().saturating_sub(tag.len() as u16);
            f.buffer_mut()
                .set_string(x, pane.y, tag, Style::new().fg(ACCENT));
        }
    } else {
        let hint = if app.sessions.is_empty() {
            "no sessions — Ctrl+A c creates one"
        } else {
            "select a session (Ctrl+A j/k)"
        };
        let y = pane.y + pane.height / 2;
        let x = pane.x + (pane.width.saturating_sub(hint.len() as u16)) / 2;
        f.buffer_mut().set_string(x, y, hint, Style::new().fg(DIM));
    }

    if let Some(modal) = &app.modal {
        draw_modal(f, modal);
    }
}

/// A centered overlay: the rename input box or the kill confirmation.
fn draw_modal(f: &mut Frame<'_>, modal: &Modal) {
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
    if let Some(pos) = cursor {
        f.set_cursor_position(pos);
    }
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

    // Session rows: two lines each.
    let list_bottom = area.bottom();
    let mut y = area.top();
    for (i, s) in app.sessions.iter().enumerate() {
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
        // Line 1: [marker][ordinal][name] … kill mark. The marker is the
        // activity dot (or the selection bar, drawn below).
        let dot = if s.attached_clients > 0 { "•" } else { " " };
        buf.set_string(area.left(), y, dot, row_bg.fg(ACCENT));
        // 1-based ordinal, right-aligned in 2 cols — it matches the Ctrl+A <n>
        // quick-switch for the first nine rows (dimmer past nine: no shortcut).
        let n = i + 1;
        let num_style = if selected {
            row_bg.fg(ACCENT)
        } else if i < 9 {
            row_bg.fg(MUTED)
        } else {
            row_bg.fg(DIM)
        };
        buf.set_string(area.left() + 1, y, format!("{n:>2}"), num_style);
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
            "PREFIX  j/k switch · 1-9 jump · c new · r rename · x kill · R reconnect · q quit · Ctrl+A literal",
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
        (
            format!("● {} sessions", app.sessions.len()),
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
    let x = area
        .right()
        .saturating_sub(status.chars().count() as u16 + 1);
    // Only draw the status when it doesn't collide with the hint.
    if x > area.left() + 1 + hint.chars().count().min(max) as u16 {
        buf.set_string(x, y, status, sstyle);
    }
}

/// Which sidebar session row (and whether its kill mark) a click lands on.
pub fn sidebar_hit(area: Rect, sessions: usize, col: u16, row: u16) -> Option<(usize, bool)> {
    let (side, _, _) = areas(area);
    if col >= side.right() - 1 || row >= side.bottom() {
        return None;
    }
    let idx = ((row.checked_sub(side.top())?) / 2) as usize;
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
    for y in 0..rows {
        for x in 0..cols {
            let cell = &snap.cells[y as usize][x as usize];
            if matches!(cell.width, CellWidth::SpacerTail | CellWidth::SpacerHead) {
                continue;
            }
            let Some(target) = buf.cell_mut(Position::new(area.x + x, area.y + y)) else {
                continue;
            };
            if cell.grapheme.is_empty() {
                target.set_symbol(" ");
            } else {
                target.set_symbol(&cell.grapheme);
            }
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    } else {
        s.to_string()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let (cols, rows) = pane_grid(Rect::new(0, 0, 120, 40));
        assert_eq!(cols, 120 - SIDEBAR_W);
        assert_eq!(rows, 39); // one row goes to the full-width keybind bar
    }

    #[test]
    fn bar_spans_the_full_width() {
        let (_, _, bar) = areas(Rect::new(0, 0, 120, 40));
        assert_eq!(bar, Rect::new(0, 39, 120, 1));
    }

    #[test]
    fn sidebar_hits_map_rows_and_kill_marks() {
        let area = Rect::new(0, 0, 120, 40);
        // First session row, name area → select.
        assert_eq!(sidebar_hit(area, 3, 2, 0), Some((0, false)));
        assert_eq!(sidebar_hit(area, 3, 2, 1), Some((0, false)));
        // Second session row.
        assert_eq!(sidebar_hit(area, 3, 2, 2), Some((1, false)));
        // Kill mark: first line of a row, near the right edge.
        assert_eq!(sidebar_hit(area, 3, SIDEBAR_W - 3, 0), Some((0, true)));
        // Beyond the list or in the pane → no hit.
        assert_eq!(sidebar_hit(area, 3, 2, 12), None);
        assert_eq!(sidebar_hit(area, 3, SIDEBAR_W + 5, 0), None);
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
            cells: vec![vec![cell("a"), cell(" "), cell(""), cell("")]],
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
}
