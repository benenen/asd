//! The search dropdown, drawn over the graph pane.
//!
//! The query on the first row, then one row per match. Every character goes
//! through `graph_view::put`, which is this crate's only clamped text writer: nothing
//! here indexes the buffer, because this draws on `asd ui`'s render thread
//! where one out-of-bounds write blanks every session's display.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Widget};

use crate::git::graph::GraphNode;
use crate::search::Search;
use crate::ui::graph_view::put;

const DIM: Color = Color::Rgb(0x8B, 0x94, 0xA2);
const ACCENT: Color = Color::Rgb(0xF3, 0xB2, 0x4C);

/// Match rows the dropdown shows at once.
///
/// It is a dropdown, not a fourth pane: a query matching most of the history
/// must not grow until it covers the graph it is meant to be searched
/// against. The list scrolls to keep the highlighted match visible, so a
/// match past this many is still reachable.
const MAX_MATCH_ROWS: usize = 10;

/// Render the dropdown into the top of `area`, which is the graph pane.
///
/// The box is only as tall as it needs to be, so the graph stays visible
/// beneath it. `nodes` is the slice the row indices in `search` came from.
/// `not_loaded` is how many commits the graph has read but not yet laid out,
/// which is how many rows the search could not see.
pub(crate) fn draw_search(
    buf: &mut Buffer,
    area: Rect,
    search: &Search,
    nodes: &[GraphNode],
    not_loaded: usize,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let matches = search.matches();
    let visible = matches.len().min(MAX_MATCH_ROWS);
    // With nothing matched there is still one row to draw, but only once
    // something has been typed: "no matches" against an empty query would be
    // a complaint about a search the user has not made yet.
    let body = if matches.is_empty() {
        usize::from(!search.query().is_empty())
    } else {
        visible
    };
    // Query row + body rows + the two border rows, never taller than `area`.
    // `body` is bounded by `MAX_MATCH_ROWS`, so the `u16` cast cannot wrap.
    let height = (body as u16 + 3).min(area.height);
    let rect = Rect::new(area.x, area.y, area.width, height);

    // Blank the whole box, border ring included, before anything is drawn
    // into it. An overlay has to be opaque, and nothing else here makes it
    // so: `Block::render` sets styles and draws border glyphs but never
    // clears the symbols between them, and `draw_rows` — which painted this
    // same region moments ago — neither pads a row to its full width nor
    // keeps its abbreviated hash out of the way (it sits at `right - 7`,
    // inside this box, since the right border is at `right - 1`). Without
    // this pass every match row carries a stray commit hash and the tail of
    // a longer graph summary.
    //
    // `Style::reset` rather than `Style::default`: `Cell::set_style` only
    // *inserts* `add_modifier` and *removes* `sub_modifier`, so a default
    // style leaves the graph's `REVERSED` selected row set underneath and a
    // blanked cell comes out as a solid block. `reset` carries
    // `sub_modifier: Modifier::all()` and explicit `Color::Reset`, which is
    // what actually clears the cell — colours as well as modifiers.
    let blank = " ".repeat(rect.width as usize);
    for y in rect.y..rect.y.saturating_add(rect.height) {
        put(buf, rect, rect.x, y, &blank, Style::reset());
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Search ")
        .border_style(Style::default().fg(ACCENT));
    let inner = block.inner(rect);
    block.render(rect, buf);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // The query row. The trailing block is a cursor: without it an empty
    // query looks like a box that failed to open rather than one waiting for
    // input.
    let x = put(
        buf,
        inner,
        inner.x,
        inner.y,
        &format!("/{}", search.query()),
        Style::default(),
    );
    put(buf, inner, x, inner.y, "▏", Style::default().fg(ACCENT));

    // The right-hand status. "3/41" says how much of the match list the cap
    // is hiding; "n not loaded" says how much of the *history* the search
    // could not see at all. Only rows the graph has laid out are searchable,
    // so without that second half a miss on a commit the reader knows exists
    // is indistinguishable from a genuine one — a silent wrong answer.
    let counter = search
        .selected()
        .map(|selected| format!("{}/{}", selected + 1, matches.len()));
    let unloaded = (not_loaded > 0).then(|| format!("{not_loaded} not loaded"));
    let status = match (counter.as_deref(), unloaded.as_deref()) {
        (Some(c), Some(u)) => Some(format!("{c} · {u}")),
        (Some(c), None) => Some(c.to_string()),
        (None, Some(u)) => Some(u.to_string()),
        (None, None) => None,
    };
    // Longest first: on a pane too narrow for the whole status the counter
    // alone still beats nothing, and neither may be painted over the text the
    // user typed — `put` clamps to the area, not to the query.
    for text in [status.as_deref(), counter.as_deref()]
        .into_iter()
        .flatten()
    {
        let w = unicode_width::UnicodeWidthStr::width(text) as u16;
        if x.saturating_add(1).saturating_add(w) <= inner.x.saturating_add(inner.width) {
            let cx = inner
                .x
                .saturating_add(inner.width)
                .saturating_sub(w)
                .max(inner.x);
            put(buf, inner, cx, inner.y, text, Style::default().fg(DIM));
            break;
        }
    }

    // Body rows start below the query row. On a box too short for any of
    // them the loop below simply runs zero times.
    let list = Rect {
        y: inner.y.saturating_add(1),
        height: inner.height.saturating_sub(1),
        ..inner
    };
    if list.height == 0 {
        return;
    }

    if matches.is_empty() {
        if !search.query().is_empty() {
            put(
                buf,
                list,
                list.x,
                list.y,
                "no matches",
                Style::default().fg(DIM),
            );
        }
        return;
    }

    // Keep the highlighted match on screen: it is the one `Enter` jumps to,
    // and a highlight the reader cannot see is worse than no highlight.
    //
    // The window is `list.height`, not `MAX_MATCH_ROWS`. The two differ
    // whenever `area` was too short for the box the cap asked for — a 24-row
    // terminal gives the graph pane about 11 rows, so `list.height` is 8
    // while the cap is 10 — and scrolling by the cap there leaves the last
    // two matches off the bottom with the highlight nowhere on screen.
    let shown = visible.min(list.height as usize);
    let selected = search.selected().unwrap_or(0);
    let scroll = (selected + 1).saturating_sub(shown);

    for (row, &node_index) in matches.iter().skip(scroll).take(shown).enumerate() {
        let y = list.y.saturating_add(row as u16);
        let is_selected = scroll + row == selected;
        let base = if is_selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        if is_selected {
            // Repaint the row full width in the highlight style, so the
            // reversed band reads as a band rather than stopping wherever the
            // summary happens to end. `put` clamps to `list`, so the blank
            // being cut to the box's full width rather than the list's is
            // harmless.
            put(buf, list, list.x, y, &blank, base);
        }
        // `matches` indexes the node slice it was ranked against. That is the
        // same slice in every real call, but a stale index must not be able
        // to panic the render thread, so it is looked up rather than assumed.
        let Some(commit) = nodes.get(node_index).and_then(|n| n.commit.as_ref()) else {
            continue;
        };
        // Only the first line: a summary carries the whole commit message in
        // this crate, and the rest of it belongs to the detail pane.
        let summary = commit.summary.lines().next().unwrap_or("");
        let x = put(buf, list, list.x, y, summary, base);
        // The author is searched too, so a row that matched on the author
        // alone would otherwise look like it had no business being here.
        let author_style = if is_selected { base } else { base.fg(DIM) };
        put(
            buf,
            list,
            x,
            y,
            &format!("  {}", commit.author),
            author_style,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::commit::CommitInfo;
    use crate::git::graph::{CellType, GraphNode};
    use crate::search::Search;

    fn node(summary: &str, author: &str) -> GraphNode {
        GraphNode {
            commit: Some(CommitInfo {
                id: gix::ObjectId::empty_blob(gix::hash::Kind::Sha1),
                parents: Vec::new(),
                summary: summary.into(),
                author: author.into(),
                time: 0,
            }),
            lane: 0,
            color_index: 0,
            cells: vec![CellType::Commit(0)],
            uncommitted: None,
        }
    }

    /// A `Search` that has already been typed into.
    fn typed(query: &str, nodes: &[GraphNode]) -> Search {
        let mut s = Search::default();
        for c in query.chars() {
            s.push(c, nodes);
        }
        s
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
    fn shows_the_query_and_one_row_per_match() {
        let nodes = vec![
            node("fix the parser", "ann"),
            node("unrelated", "bo"),
            node("parse the fixture", "cy"),
        ];
        let search = typed("parse", &nodes);
        let area = Rect::new(0, 0, 50, 20);
        let mut buf = Buffer::empty(area);
        draw_search(&mut buf, area, &search, &nodes, 0);
        let text = text_of(&buf, area);
        assert!(text.contains("/parse"), "the query is echoed:\n{text}");
        assert!(text.contains("fix the parser"), "{text}");
        assert!(text.contains("parse the fixture"), "{text}");
        assert!(
            !text.contains("unrelated"),
            "a non-match must not be listed:\n{text}"
        );
        assert!(text.contains("ann"), "the author is shown too:\n{text}");
    }

    /// The box is a dropdown over the graph, not a replacement for it: it
    /// takes only the rows it needs and leaves the rest of the pane alone.
    #[test]
    fn the_box_is_only_as_tall_as_its_content() {
        let nodes = vec![node("alpha", "x"), node("alphabet", "y")];
        let search = typed("alpha", &nodes);
        let area = Rect::new(0, 0, 40, 20);
        let mut buf = Buffer::filled(area, ratatui::buffer::Cell::new("\u{2591}"));
        draw_search(&mut buf, area, &search, &nodes, 0);
        // Two borders, the query row and two match rows: five.
        let below = Rect::new(area.x, area.y + 5, area.width, area.height - 5);
        let untouched = "\u{2591}".repeat(below.width as usize);
        for line in text_of(&buf, below).lines() {
            assert_eq!(line, untouched, "row below the box was painted");
        }
    }

    #[test]
    fn the_selected_match_is_reversed_and_the_others_are_not() {
        let nodes = vec![node("alpha", "x"), node("alphabet", "y")];
        let mut search = typed("alpha", &nodes);
        let area = Rect::new(0, 0, 40, 20);

        let mut buf = Buffer::empty(area);
        draw_search(&mut buf, area, &search, &nodes, 0);
        // Row 0 is the top border, row 1 the query, row 2 the first match.
        let reversed = |buf: &Buffer, y: u16| {
            buf[(1, y)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        };
        assert!(reversed(&buf, 2), "the first match starts highlighted");
        assert!(!reversed(&buf, 3), "the second match is not");

        search.next();
        let mut buf = Buffer::empty(area);
        draw_search(&mut buf, area, &search, &nodes, 0);
        assert!(!reversed(&buf, 2), "the highlight moved off the first");
        assert!(reversed(&buf, 3), "and onto the second");
    }

    #[test]
    fn a_query_that_matches_nothing_says_so() {
        let nodes = vec![node("alpha", "x")];
        let search = typed("zzzz", &nodes);
        let area = Rect::new(0, 0, 40, 20);
        let mut buf = Buffer::empty(area);
        draw_search(&mut buf, area, &search, &nodes, 0);
        let text = text_of(&buf, area);
        assert!(text.contains("no matches"), "{text}");
    }

    /// An empty query is a search not yet made, not a search that failed.
    #[test]
    fn an_empty_query_does_not_complain() {
        let nodes = vec![node("alpha", "x")];
        let area = Rect::new(0, 0, 40, 20);
        let mut buf = Buffer::empty(area);
        draw_search(&mut buf, area, &Search::default(), &nodes, 0);
        let text = text_of(&buf, area);
        assert!(!text.contains("no matches"), "{text}");
        assert!(text.contains('/'), "the prompt is still drawn:\n{text}");
    }

    /// The list is capped, so selecting past the cap has to scroll it — a
    /// highlight the reader cannot see is worse than none.
    ///
    /// The heights matter. A tall area gives the box the full
    /// `MAX_MATCH_ROWS`, but a real 24-row terminal gives the graph pane
    /// about 11 rows and the box then draws fewer rows than the cap. A scroll
    /// offset derived from the cap rather than from the height it actually
    /// got leaves the last matches off the bottom with the highlight nowhere
    /// on screen, and only a short area can catch that.
    #[test]
    fn the_list_scrolls_to_keep_the_selection_visible() {
        let nodes: Vec<GraphNode> = (0..40)
            .map(|i| node(&format!("alpha commit {i:02}"), "x"))
            .collect();
        let mut search = typed("alpha", &nodes);
        assert_eq!(search.matches().len(), 40);
        for _ in 0..25 {
            search.next();
        }
        // 30 rows: the box gets its full cap. 11: a 24-row terminal's graph
        // pane. 6: room for the query row and two matches, no more.
        for height in [30u16, 11, 6] {
            let area = Rect::new(0, 0, 60, height);
            let mut buf = Buffer::empty(area);
            draw_search(&mut buf, area, &search, &nodes, 0);
            let text = text_of(&buf, area);
            assert!(
                text.contains("alpha commit 25"),
                "the 26th match must be on screen in a {height}-row area:\n{text}"
            );
            assert!(
                !text.contains("alpha commit 00"),
                "the list scrolled past the first:\n{text}"
            );
        }
        let area = Rect::new(0, 0, 60, 30);
        let mut buf = Buffer::empty(area);
        draw_search(&mut buf, area, &search, &nodes, 0);
        let text = text_of(&buf, area);
        assert!(
            text.contains("26/40"),
            "the count says what is hidden:\n{text}"
        );
    }

    /// An overlay that is not opaque is not an overlay. `Block::render` draws
    /// borders and sets styles but never clears the symbols between them, and
    /// the graph was painted over this same region moments earlier — right
    /// down to an abbreviated hash that lands *inside* the box, since
    /// `draw_rows` puts it at `right - 7` and the right border sits at
    /// `right - 1`.
    ///
    /// Every other test in this file draws into an empty or sentinel-filled
    /// buffer, so none of them can see this. This one draws over a buffer
    /// that looks like a painted graph pane: a recognisable fill, styled the
    /// way `draw_rows` styles its selected row.
    #[test]
    fn the_dropdown_paints_over_whatever_was_underneath_it() {
        let nodes = vec![node("alpha", "x"), node("alphabet", "y")];
        let search = typed("alpha", &nodes);
        let area = Rect::new(0, 0, 40, 20);
        let mut under = ratatui::buffer::Cell::new("#");
        under.set_style(
            Style::default()
                .fg(Color::Rgb(0x11, 0x22, 0x33))
                .add_modifier(Modifier::REVERSED),
        );
        let mut buf = Buffer::filled(area, under);
        draw_search(&mut buf, area, &search, &nodes, 0);

        // Two borders, the query row and two match rows.
        let rect = Rect::new(area.x, area.y, area.width, 5);
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                assert_ne!(
                    buf[(x, y)].symbol(),
                    "#",
                    "the graph showed through at ({x}, {y})"
                );
            }
        }
        // The graph's own highlight must not bleed through either. Row 2 is
        // the selected match and is legitimately reversed; the query row and
        // the unselected match row are not.
        for y in [1u16, 3] {
            for x in rect.x..rect.x + rect.width {
                assert!(
                    !buf[(x, y)]
                        .style()
                        .add_modifier
                        .contains(Modifier::REVERSED),
                    "the graph's REVERSED survived at ({x}, {y})"
                );
            }
        }
        // And the box did not clear more than it drew.
        assert_eq!(buf[(0, 5)].symbol(), "#", "the box grew past its content");
    }

    /// Only rows the graph has laid out are searchable. When some are not, a
    /// miss has two possible meanings and the box has to say which.
    #[test]
    fn the_status_says_how_much_history_was_not_searched() {
        let nodes = vec![node("alpha", "x"), node("alphabet", "y")];
        let area = Rect::new(0, 0, 60, 20);

        // With matches: the counter and the warning sit side by side.
        let search = typed("alpha", &nodes);
        let mut buf = Buffer::empty(area);
        draw_search(&mut buf, area, &search, &nodes, 1_500);
        let text = text_of(&buf, area);
        assert!(text.contains("1/2"), "{text}");
        assert!(text.contains("1500 not loaded"), "{text}");

        // The case that matters most: nothing matched, and the reason may be
        // that the commit was never loaded rather than that it does not exist.
        let search = typed("zzzz", &nodes);
        let mut buf = Buffer::empty(area);
        draw_search(&mut buf, area, &search, &nodes, 1_500);
        let text = text_of(&buf, area);
        assert!(text.contains("no matches"), "{text}");
        assert!(
            text.contains("1500 not loaded"),
            "a miss must not look final when the search was partial:\n{text}"
        );

        // A fully loaded graph says nothing, because there is nothing to say.
        let search = typed("alpha", &nodes);
        let mut buf = Buffer::empty(area);
        draw_search(&mut buf, area, &search, &nodes, 0);
        let text = text_of(&buf, area);
        assert!(!text.contains("not loaded"), "{text}");
        assert!(text.contains("1/2"), "the counter still shows:\n{text}");
    }

    /// The status degrades rather than vanishing: a box too narrow for both
    /// halves keeps the counter, and neither half may cover the query.
    #[test]
    fn a_narrow_box_keeps_the_counter_and_never_covers_the_query() {
        let nodes = vec![node("alpha", "x"), node("alphabet", "y")];
        let search = typed("alpha", &nodes);
        let area = Rect::new(0, 0, 22, 20);
        let mut buf = Buffer::empty(area);
        draw_search(&mut buf, area, &search, &nodes, 1_500);
        let text = text_of(&buf, area);
        assert!(text.contains("/alpha"), "the query survives:\n{text}");
        assert!(text.contains("1/2"), "the counter still fits:\n{text}");
        assert!(
            !text.contains("not loaded"),
            "the long form does not fit and must be dropped, not clipped:\n{text}"
        );
    }

    /// Not panicking is the floor, not the guarantee. The dropdown is drawn
    /// into part of a buffer that also holds the host's own frame, so a write
    /// one row past the area is a stray glyph in someone else's session
    /// rather than a crash. This renders into a buffer with a margin of
    /// sentinel cells and checks that none of them moved.
    #[test]
    fn nothing_is_written_outside_the_area() {
        let nodes: Vec<GraphNode> = (0..30)
            .map(|i| node(&format!("alpha commit number {i:02}"), "some author"))
            .collect();
        let searches = [
            Search::default(),
            typed("zzzz", &nodes),
            typed("alpha", &nodes),
            {
                let mut s = typed("alpha", &nodes);
                for _ in 0..29 {
                    s.next();
                }
                s
            },
        ];
        for (search, not_loaded) in searches.iter().zip([0usize, 7, 0, 12_345]) {
            for &(ox, oy) in &[(0u16, 0u16), (1, 1), (5, 3)] {
                for width in 0..=12u16 {
                    for height in 0..=8u16 {
                        let area = Rect::new(ox, oy, width, height);
                        let full = Rect::new(
                            0,
                            0,
                            ox.saturating_add(width).saturating_add(3),
                            oy.saturating_add(height).saturating_add(3),
                        );
                        let mut buf = Buffer::filled(full, ratatui::buffer::Cell::new("\u{2591}"));
                        draw_search(&mut buf, area, search, &nodes, not_loaded);
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
