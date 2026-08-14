//! Live terminal pane and empty/takeover state rendering.

use asd_vt::{CellWidth, RenderSnapshot, Rgb, UnderlineKind};
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

use super::{ACCENT, DIM, Selection, TEXT, str_width, truncate};
use crate::keymap::{KeyAction, Keymap};

const ASD_WORDMARK: [&str; 4] = [
    "  __ _ ___  __| |",
    " / _` / __|/ _` |",
    "| (_| \\__ \\ (_| |",
    " \\__,_|___/\\__,_|",
];

pub(super) fn render(
    buf: &mut Buffer,
    area: Rect,
    snapshot: &RenderSnapshot,
    selection: Option<Selection>,
) {
    let rows = snapshot.rows.min(area.height);
    let cols = snapshot.cols.min(area.width);
    for y in 0..area.height {
        for x in 0..area.width {
            let Some(target) = buf.cell_mut(Position::new(area.x + x, area.y + y)) else {
                continue;
            };
            let Some(cell) =
                (y < rows && x < cols).then(|| &snapshot.cells[y as usize][x as usize])
            else {
                target.reset();
                continue;
            };
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
            let width = match cell.width {
                CellWidth::Narrow => 1,
                CellWidth::Wide => 2,
                CellWidth::SpacerTail | CellWidth::SpacerHead => unreachable!(),
            };
            target.set_diff_option(CellDiffOption::ForcedWidth(
                std::num::NonZeroU16::new(width).unwrap(),
            ));
            let mut style = cell_style(cell);
            if !cell.grapheme.is_empty() && in_selection(selection, x, y) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            target.set_style(style);
        }
        for x in cols..area.width {
            if let Some(target) = buf.cell_mut(Position::new(area.x + x, area.y + y)) {
                target.reset();
            }
        }
    }
}

fn in_selection(selection: Option<Selection>, x: u16, y: u16) -> bool {
    let Some(selection) = selection else {
        return false;
    };
    let after_start = y > selection.start.1 || (y == selection.start.1 && x >= selection.start.0);
    let before_end = y < selection.end.1 || (y == selection.end.1 && x <= selection.end.0);
    after_start && before_end
}

fn cell_style(cell: &asd_vt::CellSnapshot) -> Style {
    let mut style = Style::new()
        .fg(cell.fg.map(color).unwrap_or(Color::Reset))
        .bg(cell.bg.map(color).unwrap_or(Color::Reset));
    let flags = &cell.flags;
    if flags.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if flags.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if flags.faint {
        style = style.add_modifier(Modifier::DIM);
    }
    if flags.inverse {
        style = style.add_modifier(Modifier::REVERSED);
    }
    if flags.blink {
        style = style.add_modifier(Modifier::SLOW_BLINK);
    }
    if flags.invisible {
        style = style.add_modifier(Modifier::HIDDEN);
    }
    if flags.strikethrough {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if flags.underline != UnderlineKind::None {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

fn color(color: Rgb) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

pub(super) fn draw_empty(
    buf: &mut Buffer,
    area: Rect,
    revoked: Option<&str>,
    is_empty: bool,
    any_selectable: bool,
    keymap: &Keymap,
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
            empty_hint(is_empty, any_selectable, keymap),
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

fn empty_hint(is_empty: bool, any_selectable: bool, keymap: &Keymap) -> String {
    let create = keymap
        .invocation_hint(KeyAction::Create)
        .unwrap_or_else(|| "create unbound".to_string());
    if is_empty {
        format!("no sessions — {create} creates one")
    } else if !any_selectable {
        format!("only this UI's own session — {create} for a new one")
    } else {
        let switch = keymap
            .invocation_hints(&[KeyAction::SelectNext, KeyAction::SelectPrevious])
            .unwrap_or_else(|| "session switching unbound".to_string());
        format!("select a session ({switch})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asd_vt::{CellSnapshot, CursorSnapshot, GhosttyVt, VtBackend};
    use ratatui::backend::{Backend, CrosstermBackend};

    #[test]
    fn empty_pane_hint_distinguishes_self_only() {
        let keymap = Keymap::default();
        assert_eq!(
            empty_hint(true, false, &keymap),
            "no sessions — Ctrl+A c creates one"
        );
        let self_only = empty_hint(false, false, &keymap);
        assert!(self_only.contains("Ctrl+A c"));
        assert!(self_only.contains("own session"));
        assert_eq!(
            empty_hint(false, true, &keymap),
            "select a session (Ctrl+A j/k)"
        );
    }

    #[test]
    fn revoked_view_draws_the_asd_wordmark_and_takeover_hint() {
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        draw_empty(
            &mut buf,
            area,
            Some("review"),
            false,
            true,
            &Keymap::default(),
        );
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
    fn selection_highlights_text_but_not_blank_cells() {
        let cell = |grapheme: &str| CellSnapshot {
            grapheme: grapheme.to_string(),
            ..CellSnapshot::default()
        };
        let snapshot = RenderSnapshot {
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
        render(
            &mut buf,
            area,
            &snapshot,
            Some(Selection {
                start: (0, 0),
                end: (3, 0),
            }),
        );
        let reversed = |x: u16| {
            buf[(x, 0)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        };
        assert!(reversed(0));
        assert!(reversed(1));
        assert!(!reversed(2));
        assert!(!reversed(3));
    }

    #[test]
    fn pane_clears_beyond_a_smaller_snapshot() {
        let cell = |grapheme: &str| CellSnapshot {
            grapheme: grapheme.to_string(),
            ..CellSnapshot::default()
        };
        let snapshot = RenderSnapshot {
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
        let pristine = Buffer::empty(area)
            .cell(Position::new(0, 0))
            .unwrap()
            .clone();
        let mut buf = Buffer::empty(area);
        let stale = [(4u16, 0u16), (1, 1), (5, 1)];
        for (x, y) in stale {
            buf.cell_mut(Position::new(x, y))
                .unwrap()
                .set_symbol("X")
                .set_style(Style::new().bg(Color::Rgb(0, 95, 0)));
        }
        render(&mut buf, area, &snapshot, None);
        assert_eq!(buf.cell(Position::new(0, 0)).unwrap().symbol(), "a");
        assert_eq!(buf.cell(Position::new(1, 0)).unwrap().symbol(), "b");
        for (x, y) in stale {
            let cell = buf.cell(Position::new(x, y)).unwrap();
            assert_eq!(
                (cell.symbol(), cell.style()),
                (pristine.symbol(), pristine.style()),
                "({x},{y}) outside the snapshot kept its stale content"
            );
        }
    }

    #[test]
    fn pane_clears_wide_char_spacers() {
        let cell = |grapheme: &str, width: CellWidth| CellSnapshot {
            grapheme: grapheme.to_string(),
            width,
            ..CellSnapshot::default()
        };
        let snapshot = RenderSnapshot {
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
        let stale = [1u16, 3];
        for x in stale {
            buf.cell_mut(Position::new(x, 0))
                .unwrap()
                .set_symbol("X")
                .set_style(Style::new().bg(Color::Rgb(0, 95, 0)));
        }
        render(&mut buf, area, &snapshot, None);
        assert_eq!(buf.cell(Position::new(0, 0)).unwrap().symbol(), "中");
        assert_eq!(buf.cell(Position::new(2, 0)).unwrap().symbol(), "a");
        for x in stale {
            let cell = buf.cell(Position::new(x, 0)).unwrap();
            assert_eq!(
                (cell.symbol(), cell.style()),
                (pristine.symbol(), pristine.style()),
                "the spacer at ({x},0) kept its stale content"
            );
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

    #[test]
    fn pane_diff_uses_the_vt_width_for_emoji_presentation_graphemes() {
        let cell = |grapheme: &str, fg: Rgb| CellSnapshot {
            grapheme: grapheme.to_string(),
            fg: Some(fg),
            width: CellWidth::Narrow,
            ..CellSnapshot::default()
        };
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
        render(&mut first_buf, area, &first, None);
        let mut second_buf = Buffer::empty(area);
        render(&mut second_buf, area, &second, None);
        let mut terminal = GhosttyVt::new(3, 1, 0);
        terminal.feed(&ansi_diff(&empty, &first_buf));
        assert_eq!(terminal.render_snapshot().cells[0][1].grapheme, "A");
        terminal.feed(&ansi_diff(&first_buf, &second_buf));
        let rendered = terminal.render_snapshot();
        assert_eq!(rendered.cells[0][0].grapheme, "✔︎");
        assert_eq!(rendered.cells[0][1].grapheme, "B");
    }

    #[test]
    fn pane_diff_clears_a_line_on_a_host_that_renders_vs16_wide() {
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
        first_cells.push(CellSnapshot::default());
        let width = first_cells.len() as u16;
        let area = Rect::new(0, 0, width, 1);
        let mut first_buf = Buffer::empty(area);
        render(&mut first_buf, area, &snapshot(first_cells), None);
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
        render(&mut previous_buf, area, &snapshot(previous_cells), None);
        let mut blank_buf = Buffer::empty(area);
        render(
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
        host.feed(&emulate_wide_vs16(ansi_diff(&previous_buf, &first_buf)));
        host.feed(&emulate_wide_vs16(ansi_diff(&first_buf, &blank_buf)));
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
        let snapshot = RenderSnapshot {
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
            foreground: Rgb {
                r: 240,
                g: 241,
                b: 242,
            },
            background: Rgb {
                r: 10,
                g: 11,
                b: 12,
            },
        };
        let area = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &snapshot, None);
        let cell = &buf[(0, 0)];
        assert_eq!(cell.fg, Color::Reset);
        assert_eq!(cell.bg, Color::Reset);
    }
}
