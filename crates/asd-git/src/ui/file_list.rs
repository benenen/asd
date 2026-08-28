//! The changed-files pane.
//!
//! Every index is clamped to `area`; this draws on `asd ui`'s render thread.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Widget};

use crate::git::diff::{FileChange, FileStat};
use crate::state::DetailState;
use crate::ui::graph_view::put;

const GREEN: Color = Color::Rgb(0x79, 0xD1, 0x8C);
const RED: Color = Color::Rgb(0xE5, 0x59, 0x5E);
const DIM: Color = Color::Rgb(0x8B, 0x94, 0xA2);

/// One-letter status marker, matching `git status --short`'s vocabulary.
fn marker(change: &FileChange) -> char {
    match change {
        FileChange::Added => 'A',
        FileChange::Modified => 'M',
        FileChange::Deleted => 'D',
        FileChange::Renamed { .. } => 'R',
    }
}

/// Keep the filename when the path does not fit: a column of identical
/// directory prefixes tells the reader nothing.
fn fit_path(path: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if path.chars().count() <= max {
        return path.to_string();
    }
    let tail: String = path
        .chars()
        .rev()
        .take(max.saturating_sub(1))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

/// Render the pane, including its border.
pub fn draw_files(
    buf: &mut Buffer,
    area: Rect,
    detail: &DetailState,
    selected: usize,
    scroll: usize,
    focused: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let border = if focused {
        Style::default().fg(Color::Rgb(0xF3, 0xB2, 0x4C))
    } else {
        Style::default().fg(DIM)
    };
    let files: &[FileStat] = match detail {
        DetailState::Ready(d) => &d.files,
        _ => &[],
    };
    let title = match detail {
        DetailState::Ready(d) => format!(" Changed Files ({}) ", d.files.len()),
        _ => " Changed Files ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border);
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if files.is_empty() {
        let msg = match detail {
            DetailState::Loading => "Loading…",
            DetailState::Unavailable => "diffs unavailable",
            DetailState::Failed(_) => "diff failed",
            DetailState::Ready(_) => "no files changed",
        };
        put(buf, inner, inner.x, inner.y, msg, Style::default().fg(DIM));
        return;
    }

    // Reserve room for the marker prefix ("X  ") only. The counts suffix is
    // written after the path and is left to `put`'s own clamp: on a narrow
    // pane a lost count is a fine trade, but a truncated-away filename is
    // not, and reserving a fixed width for the counts (as a first version of
    // this did) starved the path of columns it needed on exactly the areas
    // this module's own tests exercise.
    let path_w = (inner.width as usize).saturating_sub(3);

    for (row, (index, file)) in files
        .iter()
        .enumerate()
        .skip(scroll)
        .take(inner.height as usize)
        .enumerate()
    {
        let y = inner.y + row as u16;
        let base = if index == selected && focused {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let x = put(
            buf,
            inner,
            inner.x,
            y,
            &format!("{}  ", marker(&file.change)),
            base,
        );
        let x = put(buf, inner, x, y, &fit_path(&file.path, path_w), base);
        // Each piece is written in sequence, taking the previous `put`'s
        // returned column as its own start: that is the shape the rest of
        // this crate uses, and it keeps the write in step with `put`'s
        // display-column accounting instead of re-deriving an offset by
        // searching the already-formatted text (which is measured in bytes
        // and disagrees with `put` the moment anything upstream is not
        // plain ASCII).
        if file.unreadable.is_some() {
            // Marked, not omitted: the row says why the counts are missing,
            // and the rest of the commit is listed either way.
            put(buf, inner, x, y, "  unreadable", base.fg(RED));
        } else if file.binary {
            put(buf, inner, x, y, "  binary", base.fg(DIM));
        } else {
            let x = put(
                buf,
                inner,
                x,
                y,
                &format!("  +{}", file.insertions),
                base.fg(GREEN),
            );
            put(
                buf,
                inner,
                x,
                y,
                &format!(" -{}", file.removals),
                base.fg(RED),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::{CommitDiff, FileChange, FileStat};
    use crate::state::DetailState;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn ready(files: Vec<FileStat>) -> DetailState {
        let insertions = files.iter().map(|f| f.insertions).sum();
        let removals = files.iter().map(|f| f.removals).sum();
        DetailState::Ready(CommitDiff {
            files,
            insertions,
            removals,
        })
    }

    fn stat(path: &str, change: FileChange, ins: u32, rem: u32) -> FileStat {
        FileStat {
            path: path.into(),
            change,
            insertions: ins,
            removals: rem,
            binary: false,
            unreadable: None,
        }
    }

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
    fn lists_each_file_with_its_marker_and_counts() {
        let area = Rect::new(0, 0, 50, 8);
        let mut buf = Buffer::empty(area);
        let d = ready(vec![
            stat("a.txt", FileChange::Modified, 3, 1),
            stat("b.txt", FileChange::Added, 7, 0),
        ]);
        draw_files(&mut buf, area, &d, 0, 0, false);
        let text = text_of(&buf, area);
        assert!(text.contains("M  a.txt"), "{text}");
        assert!(text.contains("A  b.txt"), "{text}");
        assert!(text.contains("+3"), "{text}");
        assert!(text.contains("-1"), "{text}");
    }

    #[test]
    fn a_binary_file_says_so_instead_of_showing_zero_counts() {
        let area = Rect::new(0, 0, 50, 6);
        let mut buf = Buffer::empty(area);
        let mut s = stat("b.bin", FileChange::Added, 0, 0);
        s.binary = true;
        draw_files(&mut buf, area, &ready(vec![s]), 0, 0, false);
        assert!(
            text_of(&buf, area).contains("binary"),
            "{}",
            text_of(&buf, area)
        );
    }

    /// An unreadable blob is neither binary nor a file with no changes:
    /// showing `+0 -0` would be a wrong answer where "unreadable" is the
    /// honest one, and the row exists at all because the rest of the commit
    /// is still listed around it.
    #[test]
    fn an_unreadable_file_says_so_instead_of_showing_zero_counts() {
        let area = Rect::new(0, 0, 50, 6);
        let mut buf = Buffer::empty(area);
        let mut bad = stat("a.txt", FileChange::Modified, 0, 0);
        bad.unreadable = Some("reading a file's blobs: loose object".into());
        let good = stat("b.txt", FileChange::Modified, 3, 1);
        draw_files(&mut buf, area, &ready(vec![bad, good]), 0, 0, false);
        let text = text_of(&buf, area);
        assert!(text.contains("unreadable"), "{text}");
        assert!(!text.contains("+0"), "no invented counts: {text}");
        assert!(text.contains("b.txt"), "the other file survives: {text}");
        assert!(text.contains("+3"), "with its own counts: {text}");
    }

    #[test]
    fn a_long_path_is_truncated_from_the_left_so_the_filename_survives() {
        let area = Rect::new(0, 0, 24, 5);
        let mut buf = Buffer::empty(area);
        let d = ready(vec![stat(
            "very/deeply/nested/dir/interesting.rs",
            FileChange::Modified,
            1,
            0,
        )]);
        draw_files(&mut buf, area, &d, 0, 0, false);
        let text = text_of(&buf, area);
        assert!(
            text.contains("interesting.rs"),
            "the filename must survive:\n{text}"
        );
    }

    #[test]
    fn loading_and_unavailable_say_so() {
        let area = Rect::new(0, 0, 40, 6);
        let mut buf = Buffer::empty(area);
        draw_files(&mut buf, area, &DetailState::Loading, 0, 0, false);
        assert!(text_of(&buf, area).contains("Loading"));

        let mut buf = Buffer::empty(area);
        draw_files(&mut buf, area, &DetailState::Unavailable, 0, 0, false);
        assert!(text_of(&buf, area).contains("unavailable"));
    }

    /// Not panicking is the floor, not the guarantee: this draws into part of
    /// a buffer that also holds the host's own frame, so a write one row past
    /// the area is a stray glyph in someone else's session rather than a
    /// crash. This sweep used to size its buffer with slack on every side and
    /// assert nothing, so a write at `area.y + area.height` landed inside it
    /// and went unnoticed; it now takes the sentinel-margin shape of
    /// `ui/help.rs` and `ui/file_diff.rs`.
    #[test]
    fn every_small_area_selection_and_scroll_is_safe() {
        let d = ready(vec![
            stat("a.txt", FileChange::Modified, 3, 1),
            stat("b/c/d.txt", FileChange::Deleted, 0, 9),
        ]);
        for &(ox, oy) in &[(0u16, 0u16), (1, 2), (5, 3)] {
            for w in 0..16u16 {
                for h in 0..7u16 {
                    for (sel, scroll) in [(0usize, 0usize), (1, 0), (99, 99)] {
                        let area = Rect::new(ox, oy, w, h);
                        let full = Rect::new(
                            0,
                            0,
                            ox.saturating_add(w).saturating_add(3),
                            oy.saturating_add(h).saturating_add(3),
                        );
                        let mut buf = Buffer::filled(full, ratatui::buffer::Cell::new("\u{2591}"));
                        draw_files(&mut buf, area, &d, sel, scroll, true);
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
