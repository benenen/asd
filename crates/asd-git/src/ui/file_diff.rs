//! One file's diff, filling the overlay.
//!
//! Line numbers on the left, then the change marker, then the text. Every
//! index is clamped to `area`: this is the render thread.
//!
//! Nothing here highlights anything. The spans arrive already styled from
//! [`crate::worker`], because highlighting is ~141 us per line — 23 ms for a
//! 60-line screenful, every frame, on the thread that paints every open
//! session. A line with no spans (a diff past
//! [`crate::worker::MAX_HIGHLIGHT_LINES`]) is painted from its own text
//! instead, so it is still numbered and readable.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Widget};

use crate::git::diff::DiffLine;
use crate::state::FileDiffState;
use crate::ui::graph_view::put;

const ADD_BG: Color = Color::Rgb(0x10, 0x2A, 0x18);
const DEL_BG: Color = Color::Rgb(0x2E, 0x14, 0x16);
const NUM: Color = Color::Rgb(0x5A, 0x64, 0x72);
const DIM: Color = Color::Rgb(0x8B, 0x94, 0xA2);
const ACCENT: Color = Color::Rgb(0xF3, 0xB2, 0x4C);
/// Columns a tab occupies. `put` measures with `unicode-width`, which gives a
/// tab no width at all, so an unexpanded tab would silently vanish and every
/// tab-indented file would come out flush left.
const TAB_WIDTH: usize = 4;

/// Decimal digits in `n`, without allocating.
///
/// The gutter is sized against every line of the diff, up to
/// [`crate::git::diff::MAX_DIFF_LINES`] of them, once per frame. Doing that
/// with `to_string().len()` would be two allocations per line — 40 000 of them
/// per frame on a large diff, on the render thread.
fn digits(n: u32) -> usize {
    n.checked_ilog10().unwrap_or(0) as usize + 1
}

/// Render the view, including its border.
///
/// Returns how many rows of diff the view had room for, which is what bounds
/// the caller's scroll offset. A view with no room returns 0.
pub fn draw_file_diff(buf: &mut Buffer, area: Rect, state: &FileDiffState, scroll: usize) -> usize {
    if area.width == 0 || area.height == 0 {
        return 0;
    }
    let title = match state {
        FileDiffState::Closed => " diff ".to_string(),
        FileDiffState::Loading(path) => format!(" {path} "),
        FileDiffState::Ready(h) => format!(" {} ", h.diff.path),
        FileDiffState::Failed(_) => " diff ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(ACCENT));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.width == 0 || inner.height == 0 {
        return 0;
    }
    let rows = inner.height as usize;

    let highlighted = match state {
        // Not reachable through `Mode::FileDiff`, which is the only thing that
        // draws this; a bordered empty box is the honest rendering of it.
        FileDiffState::Closed => return rows,
        FileDiffState::Loading(_) => {
            put(
                buf,
                inner,
                inner.x,
                inner.y,
                "Loading…",
                Style::default().fg(DIM),
            );
            return rows;
        }
        FileDiffState::Failed(msg) => {
            put(buf, inner, inner.x, inner.y, msg, Style::default().fg(DIM));
            return rows;
        }
        FileDiffState::Ready(h) => h,
    };
    let diff = &highlighted.diff;
    if diff.binary {
        put(
            buf,
            inner,
            inner.x,
            inner.y,
            "binary file",
            Style::default().fg(DIM),
        );
        return rows;
    }

    // Number columns are sized to the largest line number actually present, so
    // the gutter does not jump about as the reader scrolls.
    let num_w = diff
        .lines
        .iter()
        .map(|l| match l {
            DiffLine::Context { old, new, .. } => digits(*old).max(digits(*new)),
            DiffLine::Added { new, .. } => digits(*new),
            DiffLine::Removed { old, .. } => digits(*old),
        })
        .max()
        .unwrap_or(1);
    // A background that stopped where the text does would read as a ragged
    // block, so a changed row is filled first and painted over.
    let blank = " ".repeat(inner.width as usize);

    for (row, line) in diff
        .lines
        .iter()
        .skip(scroll)
        .take(inner.height as usize)
        .enumerate()
    {
        let y = inner.y.saturating_add(row as u16);
        let (old, new, marker, bg, text) = match line {
            DiffLine::Context { old, new, text } => (Some(*old), Some(*new), ' ', None, text),
            DiffLine::Added { new, text } => (None, Some(*new), '+', Some(ADD_BG), text),
            DiffLine::Removed { old, text } => (Some(*old), None, '-', Some(DEL_BG), text),
        };
        if let Some(b) = bg {
            put(buf, inner, inner.x, y, &blank, Style::default().bg(b));
        }
        let gutter = format!(
            "{:>w$} {:>w$} {marker} ",
            old.map(|n| n.to_string()).unwrap_or_default(),
            new.map(|n| n.to_string()).unwrap_or_default(),
            w = num_w
        );
        let base = match bg {
            Some(b) => Style::default().bg(b),
            None => Style::default(),
        };
        let mut x = put(buf, inner, inner.x, y, &gutter, base.fg(NUM));
        let right = inner.x.saturating_add(inner.width);
        // Already highlighted, on the worker thread. Highlighting here would
        // put 141 us per line on the thread that paints every session.
        match highlighted.spans.get(scroll + row) {
            Some(spans) => {
                for (style, piece) in spans {
                    x = put(buf, inner, x, y, &expand_tabs(piece), style.patch(base));
                    if x >= right {
                        break;
                    }
                }
            }
            // Past the highlighting limit, or a reply whose spans and lines
            // disagree: paint the text rather than nothing.
            None => {
                put(buf, inner, x, y, &expand_tabs(text), base);
            }
        }
    }

    if diff.truncated {
        let y = inner.y.saturating_add(inner.height.saturating_sub(1));
        put(
            buf,
            inner,
            inner.x,
            y,
            "… diff truncated",
            Style::default().fg(DIM),
        );
    }
    rows
}

/// Tabs as spaces, borrowing when there is nothing to expand.
///
/// This is a fixed width rather than real tab stops: the spans of one line are
/// painted one after another, and a stop-aware expansion would need each span
/// to know the column the previous one ended on. Fixed-width indentation is
/// what a diff view needs, and it never loses the characters.
fn expand_tabs(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains('\t') {
        std::borrow::Cow::Owned(text.replace('\t', &" ".repeat(TAB_WIDTH)))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::FileDiff;
    use crate::ui::highlight::Highlighter;
    use crate::worker::HighlightedDiff;

    /// Every symbol in `area`, row by row, as one string.
    fn buffer_text(buf: &Buffer, area: Rect) -> String {
        (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect()
    }

    /// A diff with one of each row shape, plus a wide-glyph line.
    fn sample_lines() -> Vec<DiffLine> {
        vec![
            DiffLine::Context {
                old: 1,
                new: 1,
                text: "fn main() {".to_string(),
            },
            DiffLine::Removed {
                old: 2,
                text: "\tlet x = 1;".to_string(),
            },
            DiffLine::Added {
                new: 2,
                text: "\tlet x = 2;".to_string(),
            },
            DiffLine::Context {
                old: 3,
                new: 3,
                text: "    // 中文注释 with wide glyphs".to_string(),
            },
        ]
    }

    fn sample(path: &str) -> HighlightedDiff {
        let diff = FileDiff {
            path: path.to_string(),
            lines: sample_lines(),
            binary: false,
            truncated: false,
        };
        let mut hl = Highlighter::new();
        let spans = diff
            .lines
            .iter()
            .map(|l| match l {
                DiffLine::Context { text, .. }
                | DiffLine::Added { text, .. }
                | DiffLine::Removed { text, .. } => hl.line(path, text),
            })
            .collect();
        HighlightedDiff { diff, spans }
    }

    #[test]
    fn the_gutter_carries_both_files_line_numbers() {
        let state = FileDiffState::Ready(sample("a.rs"));
        let area = Rect::new(0, 0, 60, 8);
        let mut buf = Buffer::empty(area);
        draw_file_diff(&mut buf, area, &state, 0);
        let text = buffer_text(&buf, area);

        assert!(text.contains("a.rs"), "the path is the title: {text:?}");
        assert!(text.contains("fn main() {"), "{text:?}");
        assert!(
            text.contains("2   - "),
            "a removal is numbered on the old side only: {text:?}"
        );
        assert!(
            text.contains("  2 + "),
            "an addition on the new side only: {text:?}"
        );
    }

    #[test]
    fn the_spans_are_painted_with_the_colours_the_worker_computed() {
        // The whole point of Task 10's amendment: the view paints pre-computed
        // styles. If it fell back to plain text, every cell would be default.
        let area = Rect::new(0, 0, 60, 8);
        // Everything but the border and the gutter, on the first content row.
        let text_colours = |buf: &Buffer| {
            (1..area.width - 1)
                .filter(|&x| {
                    let cell = &buf[(x, 1)];
                    cell.symbol() != " " && cell.fg != Color::Reset && cell.fg != NUM
                })
                .count()
        };

        let mut buf = Buffer::empty(area);
        draw_file_diff(&mut buf, area, &FileDiffState::Ready(sample("a.rs")), 0);
        assert!(
            text_colours(&buf) > 0,
            "the first diff row must carry syntect's colours"
        );

        // The control, so the assertion above cannot pass on the border or on
        // the gutter: with no spans the same row is painted from plain text
        // and carries no colour at all.
        let mut bare = sample("a.rs");
        bare.spans.clear();
        let mut buf = Buffer::empty(area);
        draw_file_diff(&mut buf, area, &FileDiffState::Ready(bare), 0);
        assert_eq!(text_colours(&buf), 0, "the fallback paints no colours");
    }

    #[test]
    fn a_tab_indented_line_keeps_its_indentation() {
        // `put` measures with unicode-width, which gives a tab no width, so an
        // unexpanded tab is dropped and the line comes out flush against the
        // gutter.
        let state = FileDiffState::Ready(sample("a.rs"));
        let area = Rect::new(0, 0, 60, 8);
        let mut buf = Buffer::empty(area);
        draw_file_diff(&mut buf, area, &state, 0);
        let text = buffer_text(&buf, area);
        assert!(
            text.contains(&format!("{}let x = 1;", " ".repeat(TAB_WIDTH))),
            "the tab must become spaces: {text:?}"
        );
    }

    #[test]
    fn a_line_with_no_spans_is_still_painted() {
        // Past MAX_HIGHLIGHT_LINES the spans run out. The text must still be
        // there — unstyled, but numbered and readable.
        let mut h = sample("a.rs");
        h.spans.clear();
        let state = FileDiffState::Ready(h);
        let area = Rect::new(0, 0, 60, 8);
        let mut buf = Buffer::empty(area);
        draw_file_diff(&mut buf, area, &state, 0);
        let text = buffer_text(&buf, area);
        assert!(text.contains("fn main() {"), "{text:?}");
    }

    #[test]
    fn scrolling_moves_the_window_and_stops_at_the_end() {
        let state = FileDiffState::Ready(sample("a.rs"));
        let area = Rect::new(0, 0, 60, 8);
        let mut buf = Buffer::empty(area);
        draw_file_diff(&mut buf, area, &state, 2);
        let text = buffer_text(&buf, area);
        assert!(!text.contains("fn main() {"), "scrolled past: {text:?}");
        assert!(text.contains("let x = 2;"), "{text:?}");

        // Past the end is empty, not a panic.
        let mut buf = Buffer::empty(area);
        draw_file_diff(&mut buf, area, &state, usize::MAX);
    }

    #[test]
    fn the_states_that_are_not_a_diff_say_so() {
        let area = Rect::new(0, 0, 40, 6);
        for (state, want) in [
            (FileDiffState::Loading("a.rs".into()), "Loading"),
            (FileDiffState::Failed("no such path".into()), "no such path"),
        ] {
            let mut buf = Buffer::empty(area);
            draw_file_diff(&mut buf, area, &state, 0);
            let text = buffer_text(&buf, area);
            assert!(text.contains(want), "{state:?} rendered as {text:?}");
        }

        let mut binary = sample("a.bin");
        binary.diff.binary = true;
        binary.diff.lines.clear();
        binary.spans.clear();
        let mut buf = Buffer::empty(area);
        draw_file_diff(&mut buf, area, &FileDiffState::Ready(binary), 0);
        assert!(buffer_text(&buf, area).contains("binary file"));
    }

    #[test]
    fn a_truncated_diff_says_so_on_its_last_row() {
        let mut h = sample("a.rs");
        h.diff.truncated = true;
        let area = Rect::new(0, 0, 40, 6);
        let mut buf = Buffer::empty(area);
        draw_file_diff(&mut buf, area, &FileDiffState::Ready(h), 0);
        assert!(buffer_text(&buf, area).contains("truncated"));
    }

    /// The sweep the render path lives or dies by: `asd ui` paints every
    /// session from this thread, so one out-of-bounds write blanks all of
    /// them. A border eats two columns and two rows, so every area under 3x3
    /// has a degenerate inner area — and the CJK line is there because a
    /// double-width glyph lands on the last column of an odd-width area.
    ///
    /// The only assertion is that none of this panics.
    #[test]
    fn rendering_at_any_size_does_not_panic() {
        let mut wide = sample("a.rs");
        wide.diff.truncated = true;
        let states = [
            FileDiffState::Closed,
            FileDiffState::Loading("some/deep/path/a.rs".into()),
            FileDiffState::Failed("a failure message longer than any of these areas".into()),
            FileDiffState::Ready(sample("a.rs")),
            FileDiffState::Ready(wide),
        ];
        for state in &states {
            for &(ox, oy) in &[(0u16, 0u16), (1, 1), (5, 3)] {
                for width in 0..=12u16 {
                    for height in 0..=6u16 {
                        let area = Rect::new(ox, oy, width, height);
                        // Sized to the minimum that still contains `area`,
                        // which is ratatui's own indexing precondition.
                        let mut buf = Buffer::empty(Rect::new(
                            0,
                            0,
                            ox.saturating_add(width),
                            oy.saturating_add(height),
                        ));
                        for scroll in [0usize, 1, 3, usize::MAX] {
                            draw_file_diff(&mut buf, area, state, scroll);
                        }
                    }
                }
            }
        }
    }

    /// Not panicking is the floor, not the guarantee. The overlay is drawn
    /// into part of a buffer that also holds the host's own frame, so a write
    /// one row past the area is a stray glyph in someone else's session rather
    /// than a crash — invisible to the sweep above. This renders into a buffer
    /// with a margin of sentinel cells and checks that none of them moved.
    #[test]
    fn nothing_is_written_outside_the_area() {
        let states = [
            FileDiffState::Loading("some/deep/path/a.rs".into()),
            FileDiffState::Failed("a failure message longer than any of these areas".into()),
            FileDiffState::Ready(sample("a.rs")),
        ];
        for state in &states {
            for &(ox, oy) in &[(0u16, 0u16), (1, 1), (5, 3)] {
                for width in 0..=12u16 {
                    for height in 0..=6u16 {
                        let area = Rect::new(ox, oy, width, height);
                        let full = Rect::new(
                            0,
                            0,
                            ox.saturating_add(width).saturating_add(3),
                            oy.saturating_add(height).saturating_add(3),
                        );
                        let mut buf = Buffer::filled(full, ratatui::buffer::Cell::new("\u{2591}"));
                        draw_file_diff(&mut buf, area, state, 0);
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
    }
}
