//! Commit metadata and a scrollable CommonMark message body.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::git::commit::CommitInfo;
use crate::state::DetailState;
use crate::ui::markdown;

/// Render the pane, including its border.
///
/// Returns how many rows the pane's content has, which is what bounds a
/// caller's scroll offset. A pane with nothing to draw returns 0.
pub(crate) fn draw_detail(
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
    rows.push((
        commit.summary.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let mut content: Vec<Line<'static>> = rows
        .into_iter()
        .map(|(text, style)| Line::from(Span::styled(text, style)))
        .collect();
    if !commit.body.is_empty() {
        content.push(Line::default());
        content.extend(markdown::lines(&commit.body));
    }
    let wrapped = markdown::wrap(content, inner.width);
    let count = wrapped.len();
    let visible: Vec<_> = wrapped
        .into_iter()
        .skip(scroll)
        .take(usize::from(inner.height))
        .collect();
    Paragraph::new(visible).render(inner, buf);
    count
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
            body: String::new(),
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
    fn full_commit_body_is_shown_as_markdown_and_can_scroll() {
        let fx = crate::git::fixture::Fixture::new("markdown-body");
        fx.commit("Subject\n\n## Details\n\n- **Important** change\n- `inline_code` and &lt;tag&gt;\n\n> quoted text\n\n```rust\nlet value = 42;\n```\n\nSigned-off-by: Test User");
        let repo = crate::git::repo::Repo::open(fx.path()).unwrap();
        let commit = repo.walk().unwrap().next().unwrap().unwrap();
        let area = Rect::new(0, 0, 60, 24);
        let mut buf = Buffer::empty(area);
        let rows = draw_detail(
            &mut buf,
            area,
            Some(&commit),
            &DetailState::Loading,
            0,
            true,
        );
        let text = text_of(&buf, area);
        assert!(text.contains("Important"), "body is missing: {text}");
        assert!(text.contains("let value = 42;"));
        assert!(text.contains("<tag>"));
        assert!(!text.contains("**Important**"));
        assert!(!text.contains("## Details"));
        assert!(rows > 6);
        let small = Rect::new(0, 0, 28, 6);
        let mut tail = Buffer::empty(small);
        let small_rows = draw_detail(
            &mut tail,
            small,
            Some(&commit),
            &DetailState::Loading,
            0,
            true,
        );
        draw_detail(
            &mut tail,
            small,
            Some(&commit),
            &DetailState::Loading,
            small_rows.saturating_sub(4),
            true,
        );
        assert!(text_of(&tail, small).contains("Test User"));
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
                stage: None,
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

    /// Not panicking is the floor, not the guarantee: this draws into part of
    /// a buffer that also holds the host's own frame, so a write one row past
    /// the area is a stray glyph in someone else's session rather than a
    /// crash. This sweep used to size its buffer with slack on every side and
    /// assert nothing, so a write at `area.y + area.height` landed inside it
    /// and went unnoticed; it now takes the sentinel-margin shape of
    /// `ui/help.rs` and `ui/file_diff.rs`.
    #[test]
    fn every_small_area_and_scroll_offset_is_safe() {
        for &(ox, oy) in &[(0u16, 0u16), (2, 1), (5, 3)] {
            for w in 0..14u16 {
                for h in 0..8u16 {
                    for scroll in [0usize, 1, 50] {
                        let area = Rect::new(ox, oy, w, h);
                        let full = Rect::new(
                            0,
                            0,
                            ox.saturating_add(w).saturating_add(3),
                            oy.saturating_add(h).saturating_add(3),
                        );
                        let mut buf = Buffer::filled(full, ratatui::buffer::Cell::new("\u{2591}"));
                        draw_detail(
                            &mut buf,
                            area,
                            Some(&commit()),
                            &crate::state::DetailState::Loading,
                            scroll,
                            true,
                        );
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
