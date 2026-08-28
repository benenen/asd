//! Where the overlay's three panes sit, and which one a mouse event hit.
//!
//! Recording the rectangles once per frame and routing clicks by coordinate
//! keeps hit-testing out of the drawing code, which is how keifu does it.

use ratatui::layout::Rect;

/// The overlay's panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Graph,
    Detail,
    Files,
}

/// Pane rectangles for one frame. A zero-height detail and files pane means
/// the area was too short to split and the graph took all of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayoutMap {
    pub graph: Rect,
    pub detail: Rect,
    pub files: Rect,
}

/// Below this many rows the lower half is not worth having: a two-row pane
/// shows a border and nothing else.
const MIN_SPLIT_HEIGHT: u16 = 12;

/// Split the overlay's inner area: graph on top, detail and files side by side
/// beneath it.
pub fn split(inner: Rect) -> LayoutMap {
    if inner.height < MIN_SPLIT_HEIGHT || inner.width == 0 {
        let below = inner.y.saturating_add(inner.height);
        return LayoutMap {
            graph: inner,
            detail: Rect::new(inner.x, below, inner.width, 0),
            files: Rect::new(inner.x, below, 0, 0),
        };
    }
    let lower_h = inner.height / 2;
    let graph_h = inner.height - lower_h;
    let detail_w = inner.width / 2;
    let files_w = inner.width - detail_w;
    // Same reasoning as the short-area branch above: `split` is public and a
    // caller (Task 8) may hand it any `Rect`, not only one bounded by a real
    // terminal's buffer, so these must saturate rather than wrap or panic.
    let lower_y = inner.y.saturating_add(graph_h);
    let files_x = inner.x.saturating_add(detail_w);

    LayoutMap {
        graph: Rect::new(inner.x, inner.y, inner.width, graph_h),
        detail: Rect::new(inner.x, lower_y, detail_w, lower_h),
        files: Rect::new(files_x, lower_y, files_w, lower_h),
    }
}

/// Which pane contains `(x, y)`, if any.
pub fn pane_at(map: &LayoutMap, x: u16, y: u16) -> Option<Pane> {
    let hit = |r: Rect| {
        r.width > 0
            && r.height > 0
            && x >= r.x
            && x < r.x.saturating_add(r.width)
            && y >= r.y
            && y < r.y.saturating_add(r.height)
    };
    if hit(map.graph) {
        Some(Pane::Graph)
    } else if hit(map.detail) {
        Some(Pane::Detail)
    } else if hit(map.files) {
        Some(Pane::Files)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_graph_takes_the_upper_half_and_the_lower_half_splits_in_two() {
        let map = split(Rect::new(0, 0, 100, 40));
        assert_eq!(map.graph.height + map.detail.height, 40);
        assert_eq!(map.detail.height, map.files.height);
        assert_eq!(map.detail.width + map.files.width, 100);
        // The lower panes sit side by side, below the graph.
        assert_eq!(map.detail.y, map.graph.y + map.graph.height);
        assert_eq!(map.files.y, map.detail.y);
        assert_eq!(map.files.x, map.detail.x + map.detail.width);
    }

    #[test]
    fn a_short_area_gives_everything_to_the_graph() {
        // Below this there is no room for a useful detail pane, and half of
        // four rows is not a pane, it is a sliver.
        let map = split(Rect::new(0, 0, 80, 8));
        assert_eq!(map.graph.height, 8);
        assert_eq!(map.detail.height, 0);
        assert_eq!(map.files.height, 0);
    }

    #[test]
    fn a_degenerate_area_produces_no_panes_and_does_not_panic() {
        for (w, h) in [(0, 0), (0, 40), (100, 0), (1, 1), (2, 3)] {
            let map = split(Rect::new(0, 0, w, h));
            assert!(map.graph.width <= w && map.graph.height <= h);
            assert!(map.detail.width <= w && map.files.width <= w);
        }
    }

    #[test]
    fn a_rect_at_the_u16_edge_does_not_panic() {
        // `y + height` in the short-area branch, and `y + graph_h` /
        // `x + detail_w` in the side-by-side branch, all sit at the type's
        // ceiling here: a raw `+` would panic in debug and silently wrap in
        // release. `split` is public and Task 8 may call it with any `Rect`
        // at all, not only one bounded by a real terminal's buffer, so this
        // has to hold structurally, not just for the one caller in `render`.
        for r in [
            // Short-area branch (height < MIN_SPLIT_HEIGHT).
            Rect::new(u16::MAX - 4, u16::MAX - 4, 10, 4),
            // Side-by-side branch (height >= MIN_SPLIT_HEIGHT): exercises
            // both `inner.y + graph_h` and `inner.x + detail_w`.
            Rect::new(u16::MAX - 4, u16::MAX - 4, 10, 20),
            Rect::new(0, u16::MAX, 10, 20),
            Rect::new(u16::MAX, 0, 20, 20),
        ] {
            let map = split(r);
            assert!(map.graph.width <= r.width && map.graph.height <= r.height);
            assert!(map.detail.width <= r.width && map.files.width <= r.width);
        }
    }

    #[test]
    fn a_point_maps_to_the_pane_that_contains_it() {
        let map = split(Rect::new(0, 0, 100, 40));
        assert_eq!(pane_at(&map, 5, 1), Some(Pane::Graph));
        assert_eq!(pane_at(&map, 5, map.detail.y), Some(Pane::Detail));
        assert_eq!(
            pane_at(&map, map.files.x + 1, map.files.y),
            Some(Pane::Files)
        );
        assert_eq!(pane_at(&map, 200, 200), None);
    }

    #[test]
    fn a_non_origin_area_keeps_its_offset() {
        let map = split(Rect::new(7, 3, 60, 30));
        assert_eq!(map.graph.x, 7);
        assert_eq!(map.graph.y, 3);
        assert_eq!(pane_at(&map, 8, 4), Some(Pane::Graph));
        assert_eq!(
            pane_at(&map, 0, 0),
            None,
            "outside the area belongs to no pane"
        );
    }
}
