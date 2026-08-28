//! The commit detail pane: who wrote this commit, when, how much it changed,
//! and its summary line.
//!
//! The summary line, and only that. [`CommitInfo::summary`] comes from gix's
//! `message.summary()`, which folds everything up to the first blank line into
//! one line, so a commit *body* is not shown here — nor anywhere else in the
//! overlay. Carrying it is a later phase's feature; what this doc must not do
//! is promise "its message" and leave the reader hunting for the rest.
//!
//! One consequence is worth knowing before wondering whether the scroll is
//! broken: the pane is six rows for any commit, which is shorter than any
//! realistic pane, so `Tab` to it followed by `j`/`k`/`Ctrl+d` clamps to a
//! no-op every time. The scroll is wired and correct; it has nothing to
//! scroll until there is a body to put in it.
//!
//! Every index is clamped to `area` before use. This runs on `asd ui`'s render
//! thread, where an out-of-bounds write blanks every session's display.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Widget};

use crate::git::commit::CommitInfo;
use crate::state::DetailState;
use crate::ui::graph_view::put;

/// Render the pane, including its border.
///
/// Returns how many rows the pane's content has, which is what bounds a
/// caller's scroll offset. A pane with nothing to draw returns 0.
pub fn draw_detail(
    buf: &mut Buffer,
    area: Rect,
    commit: Option<&CommitInfo>,
    detail: &DetailState,
    scroll: usize,
    focused: bool,
) -> usize {
    if area.width == 0 || area.height == 0 {
        return 0;
    }
    let border = if focused {
        Style::default().fg(Color::Rgb(0xF3, 0xB2, 0x4C))
    } else {
        Style::default().fg(Color::Rgb(0x8B, 0x94, 0xA2))
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Commit Detail ")
        .border_style(border);
    let inner = block.inner(area);
    block.render(area, buf);
    let Some(commit) = commit else { return 0 };
    if inner.width == 0 || inner.height == 0 {
        return 0;
    }

    let mut rows: Vec<(String, Style)> = Vec::new();
    let plain = Style::default();
    let dim = Style::default().fg(Color::Rgb(0x8B, 0x94, 0xA2));

    rows.push((commit.id.to_string(), dim));
    rows.push((format!("Author  {}", commit.author), plain));
    rows.push((format!("Date    {}", format_time(commit.time)), plain));
    match detail {
        DetailState::Loading => rows.push(("Loading…".into(), dim)),
        DetailState::Unavailable => rows.push(("diffs unavailable".into(), dim)),
        DetailState::Failed(msg) => rows.push((format!("diff failed: {msg}"), dim)),
        DetailState::Ready(d) => {
            let n = d.files.len();
            // Both arms must be owned: `&format!(..)` borrows a temporary
            // that is dropped at the end of this statement, which does not
            // compile, and unifying with the `else` arm's `&'static str`
            // needs a common type anyway.
            let files = if n == 1 {
                "1 file".to_string()
            } else {
                format!("{n} files")
            };
            rows.push((
                format!("{files} changed  +{} -{}", d.insertions, d.removals),
                plain,
            ));
        }
    }
    rows.push((String::new(), plain));
    // `summary` is one line by construction, as the module doc explains, so
    // this runs exactly once. It stays a loop because that is what keeps the
    // pane correct — rather than painting an escaped newline across a row —
    // on the day a later phase carries the commit body on `CommitInfo`.
    for line in commit.summary.lines() {
        rows.push((
            line.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }

    for (i, (text, style)) in rows
        .iter()
        .skip(scroll)
        .take(inner.height as usize)
        .enumerate()
    {
        let y = inner.y + i as u16;
        put(buf, inner, inner.x, y, text, *style);
    }
    rows.len()
}

/// `YYYY-MM-DD HH:MM` in the host's local time, similar to the status bar's
/// existing timestamp format (`crates/asd-tui/src/ui/bar.rs`), minus seconds.
fn format_time(seconds: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(seconds, 0).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        None => "(unknown time)".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::commit::CommitInfo;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn commit() -> CommitInfo {
        CommitInfo {
            id: gix::ObjectId::empty_blob(gix::hash::Kind::Sha1),
            parents: Vec::new(),
            summary: "a short summary".into(),
            author: "asd test".into(),
            time: 1_700_000_000,
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
    fn shows_the_hash_author_and_summary() {
        let area = Rect::new(0, 0, 50, 10);
        let mut buf = Buffer::empty(area);
        draw_detail(
            &mut buf,
            area,
            Some(&commit()),
            &crate::state::DetailState::Loading,
            0,
            false,
        );
        let text = text_of(&buf, area);
        assert!(text.contains("asd test"), "{text}");
        assert!(text.contains("a short summary"), "{text}");
        assert!(
            text.contains("e69de29"),
            "abbreviated hash missing:\n{text}"
        );
    }

    #[test]
    fn says_loading_until_the_diff_arrives() {
        let area = Rect::new(0, 0, 50, 10);
        let mut buf = Buffer::empty(area);
        draw_detail(
            &mut buf,
            area,
            Some(&commit()),
            &crate::state::DetailState::Loading,
            0,
            false,
        );
        assert!(
            text_of(&buf, area).contains("Loading"),
            "{}",
            text_of(&buf, area)
        );
    }

    #[test]
    fn shows_the_totals_once_ready() {
        let area = Rect::new(0, 0, 50, 10);
        let mut buf = Buffer::empty(area);
        let diff = crate::git::diff::CommitDiff {
            files: vec![crate::git::diff::FileStat {
                path: "a.txt".into(),
                change: crate::git::diff::FileChange::Modified,
                insertions: 3,
                removals: 1,
                binary: false,
                unreadable: None,
            }],
            insertions: 3,
            removals: 1,
        };
        draw_detail(
            &mut buf,
            area,
            Some(&commit()),
            &crate::state::DetailState::Ready(diff),
            0,
            false,
        );
        let text = text_of(&buf, area);
        assert!(text.contains("1 file"), "{text}");
        assert!(text.contains("+3"), "{text}");
        assert!(text.contains("-1"), "{text}");
    }

    #[test]
    fn a_worker_failure_is_shown_without_hiding_the_commit() {
        let area = Rect::new(0, 0, 50, 10);
        let mut buf = Buffer::empty(area);
        draw_detail(
            &mut buf,
            area,
            Some(&commit()),
            &crate::state::DetailState::Failed("object missing".into()),
            0,
            false,
        );
        let text = text_of(&buf, area);
        assert!(text.contains("object missing"), "{text}");
        assert!(
            text.contains("asd test"),
            "the commit's own facts survive:\n{text}"
        );
    }

    #[test]
    fn a_connector_row_draws_nothing_and_does_not_panic() {
        let area = Rect::new(0, 0, 50, 10);
        let mut buf = Buffer::empty(area);
        draw_detail(
            &mut buf,
            area,
            None,
            &crate::state::DetailState::Loading,
            0,
            false,
        );
        // The border still renders — this is a real pane, not blank space —
        // but with no commit to show, the interior where commit facts would
        // otherwise go must stay blank. Checking the whole bordered `area`
        // (as the original assertion did) can never be all-whitespace once a
        // border is drawn, since the corner glyphs sit at both ends of the
        // joined string and `.trim()` only strips the ends; this checks the
        // region the border actually encloses instead.
        let inner = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2);
        assert_eq!(text_of(&buf, inner).trim(), "");
    }

    #[test]
    fn every_small_area_and_scroll_offset_is_safe() {
        for w in 0..14u16 {
            for h in 0..8u16 {
                for scroll in [0usize, 1, 50] {
                    let area = Rect::new(2, 1, w, h);
                    let mut buf = Buffer::empty(Rect::new(0, 0, w + 3, h + 2));
                    draw_detail(
                        &mut buf,
                        area,
                        Some(&commit()),
                        &crate::state::DetailState::Loading,
                        scroll,
                        true,
                    );
                }
            }
        }
    }
}
