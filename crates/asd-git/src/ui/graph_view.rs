//! Painting graph rows into a buffer.
//!
//! Every index into `area` is clamped before use and every row is truncated to
//! the visible width. This runs on `asd ui`'s main thread, so an out-of-bounds
//! write here takes down every session's display at once.

use std::collections::HashMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::git::graph::{CellType, GraphNode};
use crate::git::refs::{RefInfo, RefKind};
use crate::ui::colors::lane_color;

/// Columns between the graph and the summary text.
const GAP: u16 = 1;
/// Width of the abbreviated hash column on the right, plus its leading space.
const HASH_W: u16 = 8;

/// The glyph for a cell. Rounded corners, matching keifu's default character
/// set; the vocabulary is fixed so this is total.
///
/// The graph-cell loop in `draw_rows` writes exactly one buffer cell per
/// glyph, unlike `put`, which understands double-width text. That is sound
/// only because every glyph this function returns is single-width (box
/// drawing characters and `●` are all narrow). If the vocabulary ever grows
/// to include a wide character, the graph-cell loop needs the same
/// width-aware handling `put` already has, or a wide glyph would straddle
/// two buffer cells while only one is accounted for.
pub fn cell_glyph(cell: CellType) -> char {
    match cell {
        CellType::Empty => ' ',
        CellType::Pipe(_) => '│',
        CellType::Commit(_) => '●',
        CellType::BranchRight(_) => '╭',
        CellType::BranchLeft(_) => '╮',
        CellType::MergeLeft(_) => '╯',
        CellType::Horizontal(_) => '─',
        CellType::HorizontalPipe(..) => '┼',
        CellType::TeeRight(_) => '├',
        CellType::TeeLeft(_) => '┤',
        CellType::TeeDown(..) => '┬',
        CellType::TeeUp(..) => '┴',
    }
}

/// The colour a cell paints in.
fn cell_color(cell: CellType) -> Color {
    match cell {
        CellType::Empty => Color::Reset,
        CellType::Pipe(c)
        | CellType::Commit(c)
        | CellType::BranchRight(c)
        | CellType::BranchLeft(c)
        | CellType::MergeLeft(c)
        | CellType::Horizontal(c)
        | CellType::TeeRight(c)
        | CellType::TeeLeft(c) => lane_color(c),
        // Cells a run passes *through* carry `(run, lane)`: the run is the edge
        // being followed, so it wins the colour; the lane merely passes behind
        // it. Cells where the run *terminates* carry the run's colour alone,
        // which is the arm above.
        CellType::HorizontalPipe(run, _) | CellType::TeeDown(run, _) | CellType::TeeUp(run, _) => {
            lane_color(run)
        }
    }
}

/// `[main]`, `[origin/main]`, `(v1.2)` — the decorations for one commit that
/// survive the `o`/`t` toggles. A ref hidden by a toggle is skipped rather
/// than rebuilding `decorations` itself, so flipping a toggle costs nothing
/// beyond the redraw it always causes.
fn decoration_text(refs: &[RefInfo], show_remotes: bool, show_tags: bool) -> String {
    let mut out = String::new();
    for r in refs {
        if !r.kind.visible(show_remotes, show_tags) {
            continue;
        }
        let piece = match r.kind {
            RefKind::LocalBranch | RefKind::RemoteBranch => format!("[{}] ", r.name),
            RefKind::Tag => format!("({}) ", r.name),
        };
        out.push_str(&piece);
    }
    out
}

/// Write `text` at `(x, y)`, stopping at `area`'s right edge. Returns the
/// column just past what was written.
///
/// This is the crate's single clamped text writer: every pane that draws
/// arbitrary text (commit summaries, hashes, decorations, and the panes added
/// in later tasks) goes through it rather than writing buffer cells directly.
///
/// `x` is clamped to `area`'s left edge before anything is written: a caller
/// that computes a starting column from a saturating subtraction (the hash
/// column, on a narrow area) can otherwise hand back a value that undershoots
/// `area.x`, which would write outside the intended region — or outside the
/// buffer entirely, if the buffer's own area is no wider than `area`.
pub(crate) fn put(buf: &mut Buffer, area: Rect, x: u16, y: u16, text: &str, style: Style) -> u16 {
    let mut cx = x.max(area.x);
    let right = area.x.saturating_add(area.width);
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if w == 0 {
            continue;
        }
        if cx.saturating_add(w) > right {
            break;
        }
        buf[(cx, y)].set_symbol(&ch.to_string()).set_style(style);
        // A double-width glyph owns the next cell too; blank it so the buffer
        // does not carry a stale symbol under the right half. The width check
        // above already guarantees `cx + 1 < right` whenever `w == 2`, but the
        // comparison is repeated with saturating arithmetic rather than
        // trusted, since this is exactly the kind of index math the render
        // path must never get wrong.
        if w == 2 && cx.saturating_add(1) < right {
            buf[(cx.saturating_add(1), y)]
                .set_symbol(" ")
                .set_style(style);
        }
        cx = cx.saturating_add(w);
    }
    cx
}

/// One centred line, for "no commits yet" and read failures.
pub fn draw_message(buf: &mut Buffer, area: Rect, text: &str) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let y = area.y + area.height / 2;
    let width = unicode_width::UnicodeWidthStr::width(text) as u16;
    let x = area.x + area.width.saturating_sub(width) / 2;
    put(buf, area, x, y, text, Style::default().fg(Color::DarkGray));
}

/// The `o`/`t` toggles, bundled into one argument rather than two bare
/// `bool`s so `draw_rows` stays under clippy's argument-count lint.
#[derive(Debug, Clone, Copy)]
pub struct RefToggles {
    pub show_remotes: bool,
    pub show_tags: bool,
}

/// Paint `nodes[first_row..]` into `area`, highlighting `selected`.
///
/// `toggles` is the `o`/`t` state: a decoration whose kind is currently
/// hidden is left out of the row it would have appeared on, without touching
/// `decorations` itself.
pub fn draw_rows(
    buf: &mut Buffer,
    area: Rect,
    nodes: &[GraphNode],
    decorations: &HashMap<gix::ObjectId, Vec<RefInfo>>,
    toggles: RefToggles,
    first_row: usize,
    selected: usize,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let bottom = area.y.saturating_add(area.height);
    let right = area.x.saturating_add(area.width);

    // The graph occupies as many columns as the widest visible row, capped so
    // the summary always has room.
    let visible = nodes.iter().skip(first_row).take(area.height as usize);
    let graph_w = visible
        .clone()
        .map(|n| n.cells.len())
        .max()
        .unwrap_or(0)
        .min((area.width / 3) as usize) as u16;

    for (row, node) in visible.enumerate() {
        let y = area.y.saturating_add(row as u16);
        if y >= bottom {
            break;
        }
        let index = first_row + row;
        let base = if index == selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };

        // Graph cells, truncated to the graph column budget.
        for (col, cell) in node.cells.iter().take(graph_w as usize).enumerate() {
            let x = area.x.saturating_add(col as u16);
            if x >= right {
                break;
            }
            buf[(x, y)]
                .set_symbol(&cell_glyph(*cell).to_string())
                .set_style(base.fg(cell_color(*cell)));
        }

        let Some(commit) = node.commit.as_ref() else {
            // The synthetic uncommitted-changes row has no commit either, but
            // carries its count in `uncommitted` and gets a label instead of
            // the summary/decorations/hash a real commit row draws below.
            if let Some(count) = node.uncommitted {
                let x = area.x.saturating_add(graph_w).saturating_add(GAP);
                put(
                    buf,
                    area,
                    x,
                    y,
                    &format!("{count} uncommitted changes"),
                    base,
                );
            }
            continue; // A plain connector row draws edges and nothing else.
        };

        let mut x = area.x.saturating_add(graph_w).saturating_add(GAP);
        if let Some(refs) = decorations.get(&commit.id) {
            x = put(
                buf,
                area,
                x,
                y,
                &decoration_text(refs, toggles.show_remotes, toggles.show_tags),
                base.fg(lane_color(node.color_index))
                    .add_modifier(Modifier::BOLD),
            );
        }
        // Leave room for the hash column so the summary cannot run into it.
        let summary_area = Rect {
            width: area.width.saturating_sub(HASH_W),
            ..area
        };
        put(buf, summary_area, x, y, &commit.summary, base);

        let hash = commit.id.to_string();
        // Clamped to `area.x`: on an area narrower than the hash column this
        // subtraction would otherwise undershoot the left edge, and `put`
        // only guards its right edge.
        let hash_x = right.saturating_sub(HASH_W - 1).max(area.x);
        put(
            buf,
            area,
            hash_x,
            y,
            &hash[..7.min(hash.len())],
            base.fg(Color::DarkGray),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::commit::CommitInfo;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn node(summary: &str, lane: usize, cells: Vec<CellType>) -> GraphNode {
        GraphNode {
            commit: Some(CommitInfo {
                id: gix::ObjectId::empty_blob(gix::hash::Kind::Sha1),
                parents: Vec::new(),
                summary: summary.to_string(),
                author: "asd test".into(),
                time: 1_700_000_000,
            }),
            lane,
            color_index: 0,
            cells,
            uncommitted: None,
        }
    }

    #[test]
    fn glyphs_match_the_cell_vocabulary() {
        assert_eq!(cell_glyph(CellType::Empty), ' ');
        assert_eq!(cell_glyph(CellType::Pipe(0)), '│');
        assert_eq!(cell_glyph(CellType::Commit(0)), '●');
        assert_eq!(cell_glyph(CellType::BranchRight(0)), '╭');
        assert_eq!(cell_glyph(CellType::BranchLeft(0)), '╮');
        assert_eq!(cell_glyph(CellType::MergeLeft(0)), '╯');
        assert_eq!(cell_glyph(CellType::Horizontal(0)), '─');
        assert_eq!(cell_glyph(CellType::HorizontalPipe(0, 0)), '┼');
        assert_eq!(cell_glyph(CellType::TeeRight(0)), '├');
        assert_eq!(cell_glyph(CellType::TeeLeft(0)), '┤');
        assert_eq!(cell_glyph(CellType::TeeDown(0, 0)), '┬');
        assert_eq!(cell_glyph(CellType::TeeUp(0, 0)), '┴');
    }

    /// `o`/`t` are wired to `draw_rows` itself, not just to the booleans
    /// `GitGraph` exposes: a hidden kind must not reach the buffer at all, and
    /// a local branch must survive both toggles off, since only `o` and `t`
    /// can hide anything.
    #[test]
    fn the_toggles_hide_only_their_own_kind_of_decoration() {
        let commit_id = gix::ObjectId::empty_blob(gix::hash::Kind::Sha1);
        let mut row = node("hello world", 0, vec![CellType::Commit(0)]);
        row.commit.as_mut().unwrap().id = commit_id;
        let nodes = vec![row];

        let mut decorations = HashMap::new();
        decorations.insert(
            commit_id,
            vec![
                RefInfo {
                    name: "main".to_string(),
                    target: commit_id,
                    kind: RefKind::LocalBranch,
                },
                RefInfo {
                    name: "origin/main".to_string(),
                    target: commit_id,
                    kind: RefKind::RemoteBranch,
                },
                RefInfo {
                    name: "v1".to_string(),
                    target: commit_id,
                    kind: RefKind::Tag,
                },
            ],
        );
        let area = Rect::new(0, 0, 60, 3);

        let text = |show_remotes, show_tags| {
            let mut buf = Buffer::empty(area);
            draw_rows(
                &mut buf,
                area,
                &nodes,
                &decorations,
                RefToggles {
                    show_remotes,
                    show_tags,
                },
                0,
                0,
            );
            (0..area.width)
                .map(|x| buf[(x, 0)].symbol().to_string())
                .collect::<String>()
        };

        let both = text(true, true);
        assert!(both.contains("[main]"), "{both:?}");
        assert!(both.contains("[origin/main]"), "{both:?}");
        assert!(both.contains("(v1)"), "{both:?}");

        let no_remotes = text(false, true);
        assert!(no_remotes.contains("[main]"), "{no_remotes:?}");
        assert!(!no_remotes.contains("origin/main"), "{no_remotes:?}");
        assert!(no_remotes.contains("(v1)"), "{no_remotes:?}");

        let no_tags = text(true, false);
        assert!(no_tags.contains("[main]"), "{no_tags:?}");
        assert!(no_tags.contains("[origin/main]"), "{no_tags:?}");
        assert!(!no_tags.contains("v1"), "{no_tags:?}");

        let neither = text(false, false);
        assert!(
            neither.contains("[main]"),
            "a local branch survives both toggles off: {neither:?}"
        );
        assert!(!neither.contains("origin/main"), "{neither:?}");
        assert!(!neither.contains("v1"), "{neither:?}");
    }

    #[test]
    fn draws_the_marker_and_the_summary() {
        let nodes = vec![node("hello world", 0, vec![CellType::Commit(0)])];
        let area = Rect::new(0, 0, 40, 3);
        let mut buf = Buffer::empty(area);
        draw_rows(
            &mut buf,
            area,
            &nodes,
            &Default::default(),
            RefToggles {
                show_remotes: true,
                show_tags: true,
            },
            0,
            0,
        );

        let row: String = (0..40)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect::<Vec<_>>()
            .join("");
        assert!(row.starts_with('●'), "row starts with the marker: {row:?}");
        assert!(
            row.contains("hello world"),
            "row carries the summary: {row:?}"
        );
    }

    #[test]
    fn the_uncommitted_row_draws_its_marker_and_count_not_a_summary() {
        let uncommitted_row = GraphNode {
            commit: None,
            lane: 0,
            color_index: 0,
            cells: vec![CellType::Commit(0)],
            uncommitted: Some(3),
        };
        let nodes = vec![uncommitted_row];
        let area = Rect::new(0, 0, 40, 3);
        let mut buf = Buffer::empty(area);
        draw_rows(
            &mut buf,
            area,
            &nodes,
            &Default::default(),
            RefToggles {
                show_remotes: true,
                show_tags: true,
            },
            0,
            0,
        );

        let row: String = (0..40)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect::<Vec<_>>()
            .join("");
        assert!(row.starts_with('●'), "row starts with the marker: {row:?}");
        assert!(
            row.contains("3 uncommitted changes"),
            "row carries the count: {row:?}"
        );
    }

    #[test]
    fn a_row_wider_than_the_area_is_truncated_not_sliced() {
        // A graph deeper than the pane is wide must not panic and must not
        // write outside the area.
        let cells = vec![CellType::Pipe(0); 200];
        let nodes = vec![node("deep", 0, cells)];
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        draw_rows(
            &mut buf,
            area,
            &nodes,
            &Default::default(),
            RefToggles {
                show_remotes: true,
                show_tags: true,
            },
            0,
            0,
        );
        // Reaching here without a panic is the assertion.
        assert_eq!(buf.area().width, 10);
    }

    #[test]
    fn scrolling_past_the_end_draws_nothing_and_does_not_panic() {
        let nodes = vec![node("only", 0, vec![CellType::Commit(0)])];
        let area = Rect::new(0, 0, 20, 4);
        let mut buf = Buffer::empty(area);
        draw_rows(
            &mut buf,
            area,
            &nodes,
            &Default::default(),
            RefToggles {
                show_remotes: true,
                show_tags: true,
            },
            99,
            99,
        );
        let row: String = (0..20)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(row.trim(), "", "nothing is drawn past the end: {row:?}");
    }

    #[test]
    fn a_zero_sized_area_draws_nothing() {
        let nodes = vec![node("only", 0, vec![CellType::Commit(0)])];
        let area = Rect::new(0, 0, 0, 0);
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 2));
        draw_rows(
            &mut buf,
            area,
            &nodes,
            &Default::default(),
            RefToggles {
                show_remotes: true,
                show_tags: true,
            },
            0,
            0,
        );
        assert_eq!(buf[(0, 0)].symbol(), " ");
    }

    /// A degenerate-size-and-origin sweep, not asked for by the brief. Small
    /// and zero-sized areas are exactly where index arithmetic goes wrong,
    /// and the render path panicking takes down every session's display at
    /// once (`asd ui` is a resident multiplexer, not a one-shot renderer),
    /// so this exercises every area from 0x0 up to 12x6 at several non-zero
    /// origins against a deliberately messy set of rows: a wide multi-lane
    /// row carrying every two-colour cell variant, full branch/tag/remote
    /// decorations, a summary far longer than any of the areas under test, a
    /// double-width CJK summary, a bare connector row (`commit: None`), and
    /// the synthetic uncommitted-changes row (`commit: None`, `uncommitted:
    /// Some(_)`).
    ///
    /// The origin matters as much as the size: `area.x`/`area.y` are zero in
    /// every other test in this file, but the overlay is drawn inset in
    /// production, so a non-zero origin is the normal case, not an edge
    /// case. It is also exactly the shape of the one real bug this task
    /// found (`hash_x` undershooting `area.x` on a narrow area) — that bug
    /// is fixed, but nothing short of a sweep like this one locks the fix in
    /// against a future edit to `put` or `hash_x`. Each buffer is sized to
    /// `(ox + width, oy + height)`, the minimum that still contains `area`
    /// — ratatui's own precondition for indexing into it — rather than
    /// testing what happens when that precondition is violated.
    ///
    /// The only assertion is that none of this panics.
    #[test]
    fn no_area_from_zero_to_12x6_at_several_origins_panics_on_a_realistic_row_set() {
        let commit_id = gix::ObjectId::empty_blob(gix::hash::Kind::Sha1);
        let wide_row = GraphNode {
            commit: Some(CommitInfo {
                id: commit_id,
                parents: Vec::new(),
                summary: "a summary far longer than any area under test in this sweep loop"
                    .to_string(),
                author: "asd test".into(),
                time: 1_700_000_000,
            }),
            lane: 3,
            color_index: 5,
            cells: vec![
                CellType::Pipe(0),
                CellType::TeeDown(1, 2),
                CellType::HorizontalPipe(3, 4),
                CellType::Commit(5),
                CellType::BranchLeft(6),
                CellType::TeeUp(7, 8),
                CellType::MergeLeft(9),
                CellType::Horizontal(10),
            ],
            uncommitted: None,
        };
        let connector_row = GraphNode {
            commit: None,
            lane: 0,
            color_index: 0,
            cells: vec![
                CellType::TeeRight(0),
                CellType::TeeUp(0, 1),
                CellType::MergeLeft(0),
            ],
            uncommitted: None,
        };
        // A double-width CJK summary: `put` is the only path arbitrary text
        // flows through (the fixed glyph vocabulary is all narrow, see the
        // note on `cell_glyph`), and a wide glyph straddling the right edge
        // of a narrow area is a real case for user-supplied commit text.
        let cjk_row = node("宽字符摘要超长提交信息行", 0, vec![CellType::Commit(0)]);
        // The synthetic uncommitted-changes row: no commit, so it takes the
        // same early-return path as `connector_row`, but writes a label
        // `connector_row` does not, which is new render logic this sweep
        // needs to cover independently.
        let uncommitted_row = GraphNode {
            commit: None,
            lane: 0,
            color_index: 0,
            cells: vec![CellType::Commit(0)],
            uncommitted: Some(12),
        };
        let nodes = vec![wide_row, connector_row, cjk_row, uncommitted_row];

        let mut decorations = HashMap::new();
        decorations.insert(
            commit_id,
            vec![
                RefInfo {
                    name: "main".to_string(),
                    target: commit_id,
                    kind: RefKind::LocalBranch,
                },
                RefInfo {
                    name: "origin/main".to_string(),
                    target: commit_id,
                    kind: RefKind::RemoteBranch,
                },
                RefInfo {
                    name: "v1.2".to_string(),
                    target: commit_id,
                    kind: RefKind::Tag,
                },
            ],
        );

        for &ox in &[0u16, 1, 5] {
            for &oy in &[0u16, 1, 3] {
                for width in 0..=12u16 {
                    for height in 0..=6u16 {
                        let area = Rect::new(ox, oy, width, height);
                        // Sized to the minimum that still contains `area`:
                        // ratatui's indexing precondition, which is what the
                        // whole panic-safety argument for `put`'s clamping
                        // rests on.
                        let mut buf = Buffer::empty(Rect::new(
                            0,
                            0,
                            ox.saturating_add(width),
                            oy.saturating_add(height),
                        ));
                        // Also try selecting a row and scrolling, both in and
                        // out of range, since those are the other inputs
                        // that shift index arithmetic around.
                        draw_rows(
                            &mut buf,
                            area,
                            &nodes,
                            &decorations,
                            RefToggles {
                                show_remotes: true,
                                show_tags: true,
                            },
                            0,
                            0,
                        );
                        draw_rows(
                            &mut buf,
                            area,
                            &nodes,
                            &decorations,
                            RefToggles {
                                show_remotes: true,
                                show_tags: true,
                            },
                            1,
                            2,
                        );
                        draw_rows(
                            &mut buf,
                            area,
                            &nodes,
                            &decorations,
                            RefToggles {
                                show_remotes: true,
                                show_tags: true,
                            },
                            50,
                            50,
                        );
                    }
                }
            }
        }
        // Reaching here without a panic is the assertion.
    }
}
