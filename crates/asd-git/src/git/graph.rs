//! Lane layout: turning a stream of commits into rows of drawable cells.
//!
//! Modelled on keifu's `src/git/graph.rs` (MIT), with one deliberate
//! difference. keifu lays out a fixed window and drops edges to parents outside
//! it. This builder is fed incrementally, so a lane holding an id that has not
//! been reached yet is the normal state: lanes still open at the end of what
//! has been loaded render as pipes running off the bottom edge, which is a more
//! honest picture than truncating them.

use std::collections::HashMap;

use crate::git::commit::CommitInfo;

/// What to draw in one cell of one row. The `usize` is an index into the lane
/// palette, except `HorizontalPipe`, which carries the horizontal run's colour
/// and the colour of the vertical lane it crosses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellType {
    Empty,
    /// A lane continuing straight down.
    Pipe(usize),
    /// The commit marker itself.
    Commit(usize),
    /// `╭` — a branch leaving to the up-right.
    BranchRight(usize),
    /// `╮` — a branch leaving to the up-left.
    BranchLeft(usize),
    /// `╰` — a branch joining from the down-right.
    MergeRight(usize),
    /// `╯` — a branch joining from the down-left.
    MergeLeft(usize),
    /// `─` — a horizontal run.
    Horizontal(usize),
    /// A horizontal run crossing a vertical lane: `(horizontal, pipe)`.
    HorizontalPipe(usize, usize),
    /// `├`
    TeeRight(usize),
    /// `┤`
    TeeLeft(usize),
    /// `┴` — a fork point.
    TeeUp(usize),
}

/// One drawable row.
///
/// `commit: None` marks a connector row — a row that draws edges but stands for
/// no commit. Keeping that shape means the renderer only ever paints `cells`
/// and never has to ask what a row means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNode {
    pub commit: Option<CommitInfo>,
    pub lane: usize,
    pub color_index: usize,
    pub cells: Vec<CellType>,
}

/// Accumulates rows as commits are fed in. Rows already emitted are never
/// recomputed, which is what makes paging in more history cheap.
#[derive(Debug, Default)]
pub struct GraphBuilder {
    /// The id each lane is currently tracking. `None` is a free lane.
    lanes: Vec<Option<gix::ObjectId>>,
    /// Colour currently owned by each lane, so a fork keeps its hue.
    lane_color: HashMap<usize, usize>,
    nodes: Vec<GraphNode>,
    max_lane: usize,
    next_color: usize,
}

impl GraphBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn nodes(&self) -> &[GraphNode] {
        &self.nodes
    }

    pub fn max_lane(&self) -> usize {
        self.max_lane
    }

    /// The lane tracking `id`, if any.
    fn lane_of(&self, id: &gix::ObjectId) -> Option<usize> {
        self.lanes.iter().position(|l| l.as_ref() == Some(id))
    }

    /// The first free lane, appending one when all are busy.
    fn free_lane(&mut self) -> usize {
        match self.lanes.iter().position(Option::is_none) {
            Some(l) => l,
            None => {
                self.lanes.push(None);
                self.lanes.len() - 1
            }
        }
    }

    /// The next palette index. Lane 0's colour is handed out first and never
    /// recycled, so the trunk keeps one hue for the whole history.
    fn take_color(&mut self) -> usize {
        let c = self.next_color;
        self.next_color += 1;
        c
    }

    /// Paint one row: `Pipe` for every busy lane, then the commit marker.
    fn row_cells(&self, lane: usize, color: usize) -> Vec<CellType> {
        let width = self.max_lane + 1;
        let mut cells = vec![CellType::Empty; width];
        for (i, slot) in self.lanes.iter().enumerate().take(width) {
            if slot.is_some() {
                let c = self.lane_color.get(&i).copied().unwrap_or(i);
                cells[i] = CellType::Pipe(c);
            }
        }
        if lane < cells.len() {
            cells[lane] = CellType::Commit(color);
        }
        cells
    }

    /// Add one commit, appending its row (and any connector row it needs).
    pub fn feed(&mut self, commit: CommitInfo) {
        let lane = match self.lane_of(&commit.id) {
            Some(l) => l,
            None => self.free_lane(),
        };
        self.max_lane = self.max_lane.max(lane);

        let color = match self.lane_color.get(&lane) {
            Some(c) => *c,
            None => {
                let c = self.take_color();
                self.lane_color.insert(lane, c);
                c
            }
        };

        // The commit's own lane is freed before its parents claim lanes: the
        // first parent takes this lane back, so it must be available.
        if lane < self.lanes.len() {
            self.lanes[lane] = None;
        }

        let cells = {
            // Paint with this commit's lane marked busy again so the row shows
            // the marker rather than a gap.
            if lane < self.lanes.len() {
                self.lanes[lane] = Some(commit.id);
            } else {
                while self.lanes.len() <= lane {
                    self.lanes.push(None);
                }
                self.lanes[lane] = Some(commit.id);
            }
            let cells = self.row_cells(lane, color);
            self.lanes[lane] = None;
            cells
        };

        // First parent inherits the lane. Further parents are placed in
        // Task 4; a single-parent history needs nothing more.
        if let Some(first) = commit.parents.first() {
            while self.lanes.len() <= lane {
                self.lanes.push(None);
            }
            self.lanes[lane] = Some(*first);
            self.lane_color.insert(lane, color);
        } else {
            self.lane_color.remove(&lane);
        }

        self.nodes.push(GraphNode {
            commit: Some(commit),
            lane,
            color_index: color,
            cells,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::Fixture;
    use crate::git::repo::Repo;

    /// Build a layout from a fixture, feeding every commit.
    fn layout_of(fx: &Fixture) -> GraphBuilder {
        let repo = Repo::open(fx.path()).unwrap();
        let mut builder = GraphBuilder::new();
        for commit in repo.walk().unwrap() {
            builder.feed(commit.unwrap());
        }
        builder
    }

    #[test]
    fn linear_history_uses_one_lane() {
        let fx = Fixture::new("layout-linear");
        fx.commit("first");
        fx.commit("second");
        fx.commit("third");

        let b = layout_of(&fx);
        assert_eq!(b.max_lane(), 0, "a linear history never leaves lane 0");
        assert_eq!(b.nodes().len(), 3, "no connector rows in a linear history");
        for node in b.nodes() {
            assert!(node.commit.is_some(), "every row is a real commit");
            assert_eq!(node.lane, 0);
            assert_eq!(
                node.cells.first(),
                Some(&CellType::Commit(node.color_index)),
                "lane 0 holds the commit marker"
            );
        }
    }

    #[test]
    fn an_empty_builder_has_no_rows() {
        let b = GraphBuilder::new();
        assert!(b.nodes().is_empty());
        assert_eq!(b.max_lane(), 0);
    }
}
