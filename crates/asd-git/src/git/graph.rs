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

/// What to draw in one cell of one row.
///
/// Every variant is defined by the strokes it connects, and the character in
/// its doc comment is derived from those strokes — up is toward newer commits,
/// down is toward older ones. A renderer is therefore a lookup from variant to
/// character and needs to know nothing else. Nothing here is named for where a
/// branch "goes": that reading is what let the emitted structure and the
/// documented characters drift apart in the first place.
///
/// The `usize` is an index into the lane palette. Three variants carry two:
/// where a run passes *through* a cell that a lane's vertical stroke also
/// occupies, both owners are recorded as `(run, lane)` so the renderer can
/// choose between them. A cell where a run *terminates* carries only the run's
/// colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellType {
    /// Nothing is drawn here.
    Empty,
    /// `│` — up, down. A lane running straight through the row.
    Pipe(usize),
    /// The commit marker itself.
    Commit(usize),
    /// `─` — left, right. A run over a cell no lane occupies.
    Horizontal(usize),
    /// `┼` — up, down, left, right. `(run, lane)`. A run crossing a lane that
    /// carries on both above and below this row.
    HorizontalPipe(usize, usize),
    /// `╭` — down, right. A lane that starts on this row and is reached from
    /// its right, so it sits left of the commit.
    BranchRight(usize),
    /// `╮` — down, left. A lane that starts on this row and is reached from
    /// its left, so it sits right of the commit.
    BranchLeft(usize),
    /// `╯` — up, left. A lane that ends on this row, joining leftwards into
    /// the trunk.
    MergeLeft(usize),
    /// `├` — up, down, right. A lane running through the row that a run also
    /// reaches from its right: the connector row's trunk, and a merge edge
    /// landing on a live lane left of the commit.
    TeeRight(usize),
    /// `┤` — up, down, left. A lane running through the row that a merge edge
    /// reaches from its left, so it sits right of the commit.
    TeeLeft(usize),
    /// `┬` — down, left, right. `(run, lane)`. A lane that starts on this row
    /// underneath a run passing over it.
    TeeDown(usize, usize),
    /// `┴` — up, left, right. `(run, lane)`. A lane that ends on this row
    /// underneath a run passing over it.
    TeeUp(usize, usize),
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
    /// Width is frozen at push time from the then-current `max_lane`, so rows
    /// are not uniform width once a later fork widens the graph. A renderer
    /// must pad short rows rather than assume every row is `max_lane + 1`.
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

    /// Lanes other than `keep` that are also tracking `id`. These are branches
    /// rejoining: the row before the commit draws them merging into `keep`.
    fn rejoining_lanes(&self, id: &gix::ObjectId, keep: usize) -> Vec<usize> {
        self.lanes
            .iter()
            .enumerate()
            .filter(|(i, l)| *i != keep && l.as_ref() == Some(id))
            .map(|(i, _)| i)
            .collect()
    }

    /// The connector row drawn when two or more lanes rejoin at one commit.
    /// It stands for no commit, so `commit` is `None`.
    ///
    /// Indexes `cells` unguarded, on this invariant: **every occupied lane
    /// index is at most `self.max_lane`**, so a row of `max_lane + 1` cells has
    /// room for all of them. It holds because a lane only ever becomes occupied
    /// in two places, and both fold the index into `max_lane` first — `feed`
    /// reuses the commit's own lane, which it raised `max_lane` to cover before
    /// calling here, and the extra-parent arm raises `max_lane` to whatever
    /// `free_lane` handed back. `keep` is the commit's lane and every entry in
    /// `extra` is an occupied lane, so both are in range. The `debug_assert!`
    /// at the end of `feed` checks this on every call in test builds.
    fn push_connector(&mut self, keep: usize, extra: &[usize], color: usize) {
        let width = self.max_lane + 1;
        let mut cells = vec![CellType::Empty; width];
        for (i, slot) in self.lanes.iter().enumerate().take(width) {
            if slot.is_some() {
                let c = self.lane_color.get(&i).copied().unwrap_or(i);
                cells[i] = CellType::Pipe(c);
            }
        }
        let far = extra.iter().copied().max().unwrap_or(keep);
        // The run between the trunk and the furthest rejoining lane. A lane it
        // crosses keeps its own colour underneath the run.
        for (i, cell) in cells.iter_mut().enumerate().take(far).skip(keep + 1) {
            *cell = match *cell {
                // A rejoining lane is released a few statements below, so it
                // has no stroke under this row: the run ends it rather than
                // crossing it, or it would trail off into nothing.
                CellType::Pipe(pipe) if extra.contains(&i) => CellType::TeeUp(color, pipe),
                CellType::Pipe(pipe) => CellType::HorizontalPipe(color, pipe),
                _ => CellType::Horizontal(color),
            };
        }
        // The trunk carries on down to the commit this row joins at, so it
        // needs a down stroke as well as the run leaving to its right.
        cells[keep] = CellType::TeeRight(color);
        if far != keep {
            cells[far] = CellType::MergeLeft(color);
        }
        self.nodes.push(GraphNode {
            commit: None,
            lane: keep,
            color_index: color,
            cells,
        });
    }

    /// Draw a merge edge from the commit at `lane` out to a parent's lane, and
    /// the terminal cell where it lands. `target` may sit on either side of
    /// `lane`: `free_lane` hands back the leftmost free slot, which can be to
    /// the left of a commit that took its own lane from `lane_of`.
    ///
    /// Every cell is chosen from the strokes it has to connect, which is why
    /// the row painter has to have run first: a cell still holding `Pipe` is a
    /// lane that was already live and so keeps an up stroke, while `Empty` is a
    /// lane this very merge opens and so has none. `parents` names the lanes
    /// this commit's parents landed on, which is what tells an opening lane
    /// under the run (`┬`) from bare space the run merely passes over (`─`).
    fn paint_merge_edge(
        &self,
        cells: &mut Vec<CellType>,
        lane: usize,
        target: usize,
        color: usize,
        parents: &[usize],
    ) {
        if target == lane {
            return;
        }
        while cells.len() <= target {
            cells.push(CellType::Empty);
        }
        // The cells strictly between the commit and the parent's lane.
        let (first, end) = if target > lane {
            (lane + 1, target)
        } else {
            (target + 1, lane)
        };
        for (i, cell) in cells.iter_mut().enumerate().take(end).skip(first) {
            *cell = match *cell {
                CellType::Pipe(pipe) => CellType::HorizontalPipe(color, pipe),
                // A parent's lane opens here and carries on below, so the run
                // passing over it needs a down stroke too.
                CellType::Empty if parents.contains(&i) => {
                    CellType::TeeDown(color, self.lane_color.get(&i).copied().unwrap_or(i))
                }
                CellType::Empty => CellType::Horizontal(color),
                other => other,
            };
        }
        // The terminal: a live lane keeps its up and down strokes and only
        // gains the run, while a lane opening here gains a down stroke instead.
        cells[target] = match (cells[target], target > lane) {
            (CellType::Pipe(_), true) => CellType::TeeLeft(color),
            (CellType::Pipe(_), false) => CellType::TeeRight(color),
            (_, true) => CellType::BranchLeft(color),
            (_, false) => CellType::BranchRight(color),
        };
    }

    /// Add one commit, appending its row (and any connector row it needs).
    pub fn feed(&mut self, commit: CommitInfo) {
        let lane = match self.lane_of(&commit.id) {
            Some(l) => l,
            None => self.free_lane(),
        };
        self.max_lane = self.max_lane.max(lane);

        // Two or more lanes tracking this same commit means branches rejoining.
        // Emit a connector row showing the join, then free the extra lanes.
        let rejoining = self.rejoining_lanes(&commit.id, lane);
        if !rejoining.is_empty() {
            let color = self.lane_color.get(&lane).copied().unwrap_or(lane);
            self.push_connector(lane, &rejoining, color);
            for l in rejoining {
                self.lanes[l] = None;
                self.lane_color.remove(&l);
            }
        }

        let color = match self.lane_color.get(&lane) {
            Some(c) => *c,
            None => {
                let c = self.take_color();
                self.lane_color.insert(lane, c);
                c
            }
        };

        // The commit's own lane is freed before its parents claim lanes: the
        // first parent takes this lane back, so it must be available. Painting
        // first costs the row nothing, because `row_cells` writes the commit
        // marker at `lane` whatever the lane currently holds.
        self.lanes[lane] = None;
        let mut cells = self.row_cells(lane, color);

        // The first parent inherits this lane; each further parent claims a
        // lane of its own, and the edge reaching it is the merge row's
        // horizontal run.
        let mut extra_lanes: Vec<usize> = Vec::new();
        for (index, parent) in commit.parents.iter().enumerate() {
            if index == 0 {
                self.lanes[lane] = Some(*parent);
                self.lane_color.insert(lane, color);
                continue;
            }
            // A parent already being tracked keeps its lane; the edge just
            // reaches across to it.
            let target = match self.lane_of(parent) {
                Some(existing) => existing,
                None => {
                    let l = self.free_lane();
                    self.lanes[l] = Some(*parent);
                    let c = self.take_color();
                    self.lane_color.insert(l, c);
                    l
                }
            };
            // Raised before the row is widened, so the row never grows past
            // `max_lane + 1`.
            self.max_lane = self.max_lane.max(target);
            extra_lanes.push(target);
        }
        if commit.parents.is_empty() {
            self.lane_color.remove(&lane);
        }

        // Reach out to the furthest parent lane on each side of the commit.
        // One run per side, not one per parent: a parent lane in the middle of
        // a run stays a plain crossing, which is what the octopus case draws.
        if let Some(right) = extra_lanes.iter().copied().max().filter(|t| *t > lane) {
            self.paint_merge_edge(&mut cells, lane, right, color, &extra_lanes);
        }
        if let Some(left) = extra_lanes.iter().copied().min().filter(|t| *t < lane) {
            self.paint_merge_edge(&mut cells, lane, left, color, &extra_lanes);
        }

        debug_assert!(
            self.lanes
                .iter()
                .enumerate()
                .all(|(i, slot)| slot.is_none() || i <= self.max_lane),
            "an occupied lane sits past max_lane, so a row cannot hold it"
        );
        debug_assert!(
            cells.len() <= self.max_lane + 1,
            "a row grew past max_lane + 1"
        );

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

    /// Summaries of the real-commit rows, in order. Connector rows are skipped.
    fn summaries(b: &GraphBuilder) -> Vec<String> {
        b.nodes()
            .iter()
            .filter_map(|n| n.commit.as_ref().map(|c| c.summary.clone()))
            .collect()
    }

    #[test]
    fn a_fork_and_merge_uses_two_lanes() {
        let fx = Fixture::new("layout-fork");
        fx.commit("base");
        fx.branch("side");
        fx.commit("on side");
        fx.checkout("main");
        fx.commit("on main");
        fx.merge("side", "merge side");

        let b = layout_of(&fx);
        assert_eq!(b.max_lane(), 1, "one fork needs exactly two lanes");
        assert_eq!(summaries(&b).len(), 4, "four commits, connectors excluded");

        // The merge row is the newest; its cells reach across to lane 1.
        let merge_row = b
            .nodes()
            .iter()
            .find(|n| n.commit.as_ref().is_some_and(|c| c.summary == "merge side"))
            .expect("merge row exists");
        assert!(
            merge_row
                .cells
                .iter()
                .any(|c| matches!(c, CellType::Horizontal(_) | CellType::BranchLeft(_))),
            "a merge row draws an edge to the second parent: {:?}",
            merge_row.cells
        );

        // The base commit is where the two lanes rejoin, so a connector row
        // must have been emitted before it.
        let base_index = b
            .nodes()
            .iter()
            .position(|n| n.commit.as_ref().is_some_and(|c| c.summary == "base"))
            .expect("base row exists");
        assert!(
            b.nodes()[..base_index].iter().any(|n| n.commit.is_none()),
            "the fork rejoining emits a connector row"
        );
    }

    #[test]
    fn an_octopus_merge_releases_every_extra_lane() {
        let fx = Fixture::new("layout-octopus");
        fx.commit("base");
        for name in ["a", "b", "c"] {
            fx.checkout("main");
            fx.branch(name);
            fx.commit(&format!("on {name}"));
        }
        fx.checkout("main");
        fx.merge_many(&["a", "b", "c"], "octopus");

        let b = layout_of(&fx);
        // `merge_many` advances the fixture clock, so the octopus is strictly
        // newer than every branch tip it merges and is therefore fed first.
        // Without that the two share a commit time and the layout depends on
        // how the walk breaks the tie.
        assert_eq!(
            b.nodes()
                .first()
                .and_then(|n| n.commit.as_ref())
                .map(|c| c.summary.as_str()),
            Some("octopus"),
            "the merge is the newest commit, so it is fed before its parents"
        );
        let octopus = b
            .nodes()
            .iter()
            .find(|n| n.commit.as_ref().is_some_and(|c| c.summary == "octopus"))
            .expect("octopus row exists");
        assert_eq!(
            octopus.commit.as_ref().unwrap().parents.len(),
            4,
            "three side branches plus main"
        );
        assert!(b.max_lane() >= 3, "four parents need at least four lanes");

        // Once everything has rejoined at `base`, the trunk is alone again.
        let last = b.nodes().last().expect("rows exist");
        assert_eq!(last.lane, 0, "the root commit sits on the trunk");
    }

    /// A merge whose second parent is already tracked must tee into that lane
    /// instead of painting over it, or the lane vanishes for one row.
    #[test]
    fn a_merge_onto_a_live_lane_keeps_that_lane() {
        let fx = Fixture::new("layout-live-lane");
        fx.commit("base");
        fx.branch("side");
        fx.commit("on side");
        fx.checkout("main");
        fx.commit("on main");
        fx.merge("side", "merge side");
        // Both branches carry on past the merge, so both are fed before it and
        // the second parent already owns a lane by the time the merge lands.
        // `main` moves last, so the merge keeps lane 0 and reaches rightwards.
        fx.checkout("side");
        fx.commit("side goes on");
        fx.checkout("main");
        fx.commit("after merge");

        let b = layout_of(&fx);
        let at = b
            .nodes()
            .iter()
            .position(|n| n.commit.as_ref().is_some_and(|c| c.summary == "merge side"))
            .expect("merge row exists");
        let merge_row = &b.nodes()[at];
        assert_eq!(merge_row.lane, 0, "the merge keeps the trunk");
        assert!(
            matches!(merge_row.cells.get(1), Some(CellType::TeeLeft(_))),
            "the edge tees into the live lane rather than replacing it: {:?}",
            merge_row.cells
        );
        // Lane 1 really is live on both sides of the merge row.
        assert!(
            matches!(
                b.nodes()[at - 1].cells.get(1),
                Some(CellType::Pipe(_) | CellType::Commit(_))
            ),
            "lane 1 runs into the merge row from above: {:?}",
            b.nodes()[at - 1].cells
        );
        assert!(
            b.nodes()[at + 1..]
                .iter()
                .any(|n| n.lane == 1 && n.commit.is_some()),
            "lane 1 carries a commit below the merge row"
        );
    }

    /// A parent lane can sit to the left of the merge commit's own lane. The
    /// run has to reach it, and must not erase it either.
    #[test]
    fn a_merge_edge_reaches_a_parent_lane_to_its_left() {
        let fx = Fixture::new("layout-left-reach");
        fx.commit("base");
        fx.branch("side");
        fx.commit("on side");
        fx.checkout("main");
        fx.commit("on main");
        fx.merge("side", "merge side");
        // `side` carries on past the merge and is the newest commit, so it is
        // fed first and takes lane 0. The merge then lands on lane 1 with its
        // second parent already tracked to its left.
        fx.checkout("side");
        fx.commit("side goes on");

        let b = layout_of(&fx);
        let merge_row = b
            .nodes()
            .iter()
            .find(|n| n.commit.as_ref().is_some_and(|c| c.summary == "merge side"))
            .expect("merge row exists");
        assert_eq!(merge_row.lane, 1, "the merge is not on the leftmost lane");
        assert!(
            matches!(merge_row.cells.first(), Some(CellType::TeeRight(_))),
            "the edge reaches left into the parent's live lane: {:?}",
            merge_row.cells
        );
    }

    /// The run between a commit and a parent's lane crosses whatever lies
    /// between it, in either direction, and its terminal records whether the
    /// lane starts here or was already live.
    #[test]
    fn a_merge_run_crosses_the_lanes_between_it_and_the_parent() {
        let b = GraphBuilder::new();

        // Reaching right, over one live lane and one empty cell that no parent
        // claims, onto a lane that is already live: the terminal keeps that
        // lane's up and down strokes and adds the run arriving from its left.
        let mut cells = vec![
            CellType::Commit(3),
            CellType::Pipe(8),
            CellType::Empty,
            CellType::Pipe(9),
        ];
        b.paint_merge_edge(&mut cells, 0, 3, 3, &[3]);
        assert_eq!(
            cells,
            vec![
                CellType::Commit(3),
                CellType::HorizontalPipe(3, 8),
                CellType::Horizontal(3),
                CellType::TeeLeft(3),
            ]
        );

        // Reaching left, onto a lane that is not live yet: no up stroke, so a
        // corner rather than a tee, and it opens toward the run on its right.
        let mut cells = vec![CellType::Empty, CellType::Pipe(8), CellType::Commit(3)];
        b.paint_merge_edge(&mut cells, 2, 0, 3, &[0]);
        assert_eq!(
            cells,
            vec![
                CellType::BranchRight(3),
                CellType::HorizontalPipe(3, 8),
                CellType::Commit(3),
            ]
        );
    }

    /// The two rows an octopus draws, cell by cell. Between them they contain
    /// every variant this layer emits for a fan-out and a rejoin, and both were
    /// drawing strokes into nothing before: the lanes opening under the merge
    /// run had no down stroke, and the lanes dying under the connector run were
    /// painted as full crossings.
    #[test]
    fn an_octopus_draws_its_fan_out_and_its_rejoin() {
        let fx = Fixture::new("layout-octopus-cells");
        fx.commit("base");
        for name in ["a", "b", "c"] {
            fx.checkout("main");
            fx.branch(name);
            fx.commit(&format!("on {name}"));
        }
        fx.checkout("main");
        fx.merge_many(&["a", "b", "c"], "octopus");

        let b = layout_of(&fx);
        let octopus = b
            .nodes()
            .iter()
            .find(|n| n.commit.as_ref().is_some_and(|c| c.summary == "octopus"))
            .expect("octopus row exists");
        // `●┬┬╮`: the commit, two lanes opening under the run, and the furthest
        // parent's lane opening toward the run on its left.
        assert!(
            matches!(
                octopus.cells.as_slice(),
                [
                    CellType::Commit(_),
                    CellType::TeeDown(..),
                    CellType::TeeDown(..),
                    CellType::BranchLeft(_)
                ]
            ),
            "the fan-out opens a lane under every crossing: {:?}",
            octopus.cells
        );

        let connector = b
            .nodes()
            .iter()
            .find(|n| n.commit.is_none())
            .expect("connector row exists");
        // `├┴┴╯`: the trunk carrying on down to `base`, two lanes ending under
        // the run, and the furthest one turning up-left into it.
        assert!(
            matches!(
                connector.cells.as_slice(),
                [
                    CellType::TeeRight(_),
                    CellType::TeeUp(..),
                    CellType::TeeUp(..),
                    CellType::MergeLeft(_)
                ]
            ),
            "the rejoin ends every lane it joins: {:?}",
            connector.cells
        );
        // Nothing survives the connector but the trunk, so no row below it may
        // draw in a lane the connector just closed.
        let at = b
            .nodes()
            .iter()
            .position(|n| n.commit.is_none())
            .expect("connector row exists");
        for node in &b.nodes()[at + 1..] {
            assert!(
                node.cells
                    .iter()
                    .skip(1)
                    .all(|c| matches!(c, CellType::Empty)),
                "a lane the connector closed is drawn again below it: {:?}",
                node.cells
            );
        }
    }

    #[test]
    fn every_row_is_at_most_max_lane_plus_one_wide() {
        let fx = Fixture::new("layout-width");
        fx.commit("base");
        fx.branch("side");
        fx.commit("on side");
        fx.checkout("main");
        fx.commit("on main");
        fx.merge("side", "merge side");

        let b = layout_of(&fx);
        for node in b.nodes() {
            assert!(
                node.cells.len() <= b.max_lane() + 1,
                "row is {} cells but max_lane is {}: {:?}",
                node.cells.len(),
                b.max_lane(),
                node.cells
            );
        }
    }
}
