//! The help popup, drawn over the whole overlay.
//!
//! A fixed key table, centred in whatever area it is given and clamped to it.
//! Every character goes through [`put`], the crate's only clamped text
//! writer: nothing here indexes the buffer, because this draws on `asd ui`'s
//! render thread, where one out-of-bounds write blanks every session's
//! display at once.
//!
//! This overlay draws on top of live content — the three panes are still
//! rendered underneath it — which is exactly the shape that cost the search
//! dropdown a critical: `Block::render` only sets styles and draws border
//! glyphs, it never clears the symbols between them, so a transparent popup
//! lets the graph's text and its `REVERSED` selected-row styling show
//! through. The fix, copied from `ui/search.rs`, is a blanking pass with
//! `Style::reset()` over the *whole* rect, border ring included, before the
//! block or any text is drawn. See `draw_help` for why each half of that
//! matters.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Widget};

use crate::ui::graph_view::put;

const DIM: Color = Color::Rgb(0x8B, 0x94, 0xA2);
const ACCENT: Color = Color::Rgb(0xF3, 0xB2, 0x4C);
/// Columns between the key column and the description that follows it.
const GAP: u16 = 2;

/// The key table, in the order shown. Grouped by what the keys act on, not
/// alphabetised: a reader scanning for "how do I search" finds `/` faster
/// next to the other keys that move the selection than they would between
/// `Enter` and `g`.
const KEYS: &[(&str, &str)] = &[
    ("j/k, ↑/↓", "move the focused pane"),
    ("Ctrl+d/Ctrl+u", "half page down/up"),
    ("PageDown/PageUp", "page down/up"),
    ("g/Home, G/End", "jump to newest/oldest"),
    ("@", "jump to HEAD"),
    ("[ / ]", "jump between decorated commits"),
    ("Tab, Shift+Tab", "cycle focus: graph, detail, files"),
    ("Enter", "open the selected file's diff"),
    ("/", "search commit summaries and authors"),
    ("o / t", "toggle remote branches / tags"),
    ("y", "copy the selected commit's hash"),
    ("R", "re-read the repository"),
    ("?", "show this help"),
    ("q, Esc", "close"),
];

/// Render the popup, centred in `area` and never larger than it.
pub fn draw_help(buf: &mut Buffer, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let key_w = KEYS
        .iter()
        .map(|(k, _)| unicode_width::UnicodeWidthStr::width(*k))
        .max()
        .unwrap_or(0);
    let desc_w = KEYS
        .iter()
        .map(|(_, d)| unicode_width::UnicodeWidthStr::width(*d))
        .max()
        .unwrap_or(0);
    // The two columns, the gap between them, and the two border columns —
    // clamped to `area`, exactly like the search dropdown clamps its height
    // to the graph pane rather than assuming it always has room.
    let content_w = key_w + GAP as usize + desc_w;
    let width = (content_w as u16).saturating_add(2).min(area.width);
    let height = (KEYS.len() as u16).saturating_add(2).min(area.height);

    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    let rect = Rect::new(x, y, width, height);

    // Blank the whole rect, border ring included, before anything is drawn
    // into it — the same pass `ui/search.rs` uses and for the same reason.
    //
    // `Style::reset()` rather than `Style::default()`: `Cell::set_style` only
    // *assigns* fg/bg on `Some` and only *inserts* `add_modifier`/*removes*
    // `sub_modifier`, so a default-styled blank leaves an underlying
    // `REVERSED` selected-row cell exactly as reversed as it was — it comes
    // out a solid block, not cleared. `reset` carries `Color::Reset` and
    // `sub_modifier: Modifier::all()`, which is what actually clears a cell.
    //
    // The whole `rect`, not `block.inner(rect)`: `Block::render` calls
    // `buf.set_style(area, ..)` with a default style, and its border glyphs
    // carry only a foreground colour, so a `REVERSED` cell under the border
    // ring survives untouched if only the inner area is blanked first.
    let blank = " ".repeat(rect.width as usize);
    for row in rect.y..rect.y.saturating_add(rect.height) {
        put(buf, rect, rect.x, row, &blank, Style::reset());
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(Style::default().fg(ACCENT));
    let inner = block.inner(rect);
    block.render(rect, buf);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    for (row, (key, desc)) in KEYS.iter().enumerate() {
        let y = inner.y.saturating_add(row as u16);
        if y >= inner.y.saturating_add(inner.height) {
            break;
        }
        // Padded to `key_w` so every description lines up in one column,
        // whatever the width of the key label on its own row.
        let padded = format!("{key:<width$}", width = key_w);
        let x = put(buf, inner, inner.x, y, &padded, Style::default().fg(ACCENT));
        put(
            buf,
            inner,
            x.saturating_add(GAP),
            y,
            desc,
            Style::default().fg(DIM),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(buf: &Buffer, area: Rect) -> String {
        (area.y..area.y + area.height)
            .map(|y| {
                (area.x..area.x + area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn shows_the_key_table() {
        let area = Rect::new(0, 0, 90, 20);
        let mut buf = Buffer::empty(area);
        draw_help(&mut buf, area);
        let text = text_of(&buf, area);
        assert!(text.contains("Help"), "the popup has its title:\n{text}");
        assert!(text.contains('?'), "the help key itself is listed:\n{text}");
        assert!(
            text.contains("jump between decorated commits"),
            "the bracket keys this task added are documented:\n{text}"
        );
        assert!(
            text.contains("toggle remote branches / tags"),
            "the o/t toggles are documented:\n{text}"
        );
    }

    #[test]
    fn the_popup_is_centred_in_its_area() {
        let area = Rect::new(0, 0, 90, 20);
        let mut buf = Buffer::empty(area);
        draw_help(&mut buf, area);
        // The top-left border corner must not be at the area's own corner:
        // a popup that fills or hugs the edge of a generously sized area is
        // not "centred", whatever else it draws correctly.
        assert_ne!(
            buf[(0, 0)].symbol(),
            "┌",
            "the popup must not hug the corner"
        );
    }

    /// An overlay that is not opaque is not an overlay. `Block::render` draws
    /// borders and sets styles but never clears the symbols between them, and
    /// the panes underneath are freshly painted graph rows, complete with a
    /// `REVERSED` selected row, the moment before this draws over them.
    ///
    /// This fills the buffer with a recognisable styled character first, then
    /// asserts none of it survives inside the popup and that a cell just
    /// outside it is untouched — the same shape as
    /// `ui/search.rs`'s `the_dropdown_paints_over_whatever_was_underneath_it`.
    #[test]
    fn the_popup_paints_over_whatever_was_underneath_it() {
        // Wide enough that the popup sits with margin on every side: the
        // "outside" checks below need cells that are genuinely outside the
        // popup, not the popup's own edge wrapping around at 0.
        let area = Rect::new(0, 0, 90, 20);
        let mut under = ratatui::buffer::Cell::new("#");
        under.set_style(
            Style::default()
                .fg(Color::Rgb(0x11, 0x22, 0x33))
                .add_modifier(ratatui::style::Modifier::REVERSED),
        );
        let mut buf = Buffer::filled(area, under);
        draw_help(&mut buf, area);

        let key_w = KEYS
            .iter()
            .map(|(k, _)| unicode_width::UnicodeWidthStr::width(*k))
            .max()
            .unwrap_or(0);
        let desc_w = KEYS
            .iter()
            .map(|(_, d)| unicode_width::UnicodeWidthStr::width(*d))
            .max()
            .unwrap_or(0);
        let width = ((key_w + GAP as usize + desc_w) as u16 + 2).min(area.width);
        let height = (KEYS.len() as u16 + 2).min(area.height);
        let x = area.x + (area.width - width) / 2;
        let y = area.y + (area.height - height) / 2;
        let rect = Rect::new(x, y, width, height);

        for py in rect.y..rect.y + rect.height {
            for px in rect.x..rect.x + rect.width {
                assert_ne!(
                    buf[(px, py)].symbol(),
                    "#",
                    "the pane underneath showed through at ({px}, {py})"
                );
                assert!(
                    !buf[(px, py)]
                        .style()
                        .add_modifier
                        .contains(ratatui::style::Modifier::REVERSED),
                    "the pane's REVERSED survived at ({px}, {py})"
                );
            }
        }
        // The area is chosen wide enough that the popup has margin on every
        // side; otherwise `saturating_sub(1)` below would land back on the
        // popup's own edge instead of a cell genuinely outside it.
        assert!(
            rect.x > 0 && rect.y > 0,
            "the popup must have margin: {rect:?}"
        );
        assert!(
            rect.x + rect.width < area.width && rect.y + rect.height < area.height,
            "the popup must have margin: {rect:?}"
        );
        // A cell just outside the popup, on every side, must be untouched.
        let outside = [
            (rect.x - 1, rect.y),
            (rect.x + rect.width, rect.y),
            (rect.x, rect.y - 1),
            (rect.x, rect.y + rect.height),
        ];
        for (px, py) in outside {
            assert_eq!(
                buf[(px, py)].symbol(),
                "#",
                "the popup grew past its content at ({px}, {py})"
            );
        }
    }

    /// Not panicking is the floor, not the guarantee: this draws into part of
    /// a buffer that also holds the host's own frame, so a write one row past
    /// the area is a stray glyph in someone else's session rather than a
    /// crash. Same sentinel-margin shape as
    /// `ui/search.rs::nothing_is_written_outside_the_area` and
    /// `ui/file_diff.rs::nothing_is_written_outside_the_area`.
    #[test]
    fn nothing_is_written_outside_the_area() {
        for &(ox, oy) in &[(0u16, 0u16), (1, 1), (5, 3)] {
            for width in 0..=20u16 {
                for height in 0..=16u16 {
                    let area = Rect::new(ox, oy, width, height);
                    let full = Rect::new(
                        0,
                        0,
                        ox.saturating_add(width).saturating_add(3),
                        oy.saturating_add(height).saturating_add(3),
                    );
                    let mut buf = Buffer::filled(full, ratatui::buffer::Cell::new("\u{2591}"));
                    draw_help(&mut buf, area);
                    for y in full.y..full.y + full.height {
                        for x in full.x..full.x + full.width {
                            if area.contains(ratatui::layout::Position::new(x, y)) {
                                continue;
                            }
                            assert_eq!(
                                buf[(x, y)].symbol(),
                                "\u{2591}",
                                "wrote to ({x}, {y}), outside {area:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_zero_sized_area_draws_nothing() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 2));
        draw_help(&mut buf, Rect::new(0, 0, 0, 0));
        assert_eq!(buf[(0, 0)].symbol(), " ");
    }
}
