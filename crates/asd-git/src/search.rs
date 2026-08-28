//! Fuzzy search over the loaded rows.
//!
//! Ranking is pure and lives here so it can be tested without a terminal; the
//! dropdown that shows the results lives in [`crate::ui::search`].
//!
//! Only rows already paged in are searchable. The graph loads history a page
//! at a time, so a commit far enough back that nothing has pulled it in yet
//! cannot be found. Nothing here reads the repository: this runs from
//! `on_key`, on the render thread, and a walk of the whole history per
//! keystroke is exactly what that thread must not do.
//!
//! What it does do is still proportional to the loaded rows. Measured per
//! keystroke: 0.9 ms over the 500 rows `PAGE_FIRST` lays out, and about 56 ms
//! in a release build over the ~14 500 rows one `G` loads. The workspace
//! `[profile.dev.package.fuzzy-matcher]` entry is what keeps the first number
//! from being 6.8 ms in a dev build; the second needs the haystacks cached on
//! the node or the ranking moved to the worker, which is a task rather than a
//! patch.

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use crate::git::graph::GraphNode;

/// Row indices whose commit matches `query`, best first.
///
/// An empty query matches nothing: an empty dropdown is a clearer signal than
/// every row at once.
pub fn rank(nodes: &[GraphNode], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, usize)> = nodes
        .iter()
        .enumerate()
        .filter_map(|(i, node)| {
            // Connector rows and the uncommitted row stand for no commit.
            let commit = node.commit.as_ref()?;
            let haystack = format!("{} {}", commit.summary, commit.author);
            matcher
                .fuzzy_match(&haystack, query)
                .map(|score| (score, i))
        })
        .collect();
    // Best score first; ties keep the graph's own order, newest first.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// The search box's state.
///
/// `selected` indexes `matches`, not `nodes`. The two are easy to confuse and
/// mean different things: [`Search::selected`] is which row of the dropdown is
/// highlighted, [`Search::selected_row`] is the graph row that dropdown entry
/// points at.
#[derive(Debug, Default, Clone)]
pub struct Search {
    query: String,
    matches: Vec<usize>,
    selected: usize,
}

impl Search {
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Row indices, best match first.
    pub fn matches(&self) -> &[usize] {
        &self.matches
    }

    /// Which entry of the dropdown is highlighted. `None` when nothing matched.
    pub fn selected(&self) -> Option<usize> {
        self.matches.get(self.selected).map(|_| self.selected)
    }

    /// The graph row the highlighted entry refers to, for jumping to it.
    pub fn selected_row(&self) -> Option<usize> {
        self.matches.get(self.selected).copied()
    }

    pub fn push(&mut self, c: char, nodes: &[GraphNode]) {
        self.query.push(c);
        self.rerank(nodes);
    }

    pub fn backspace(&mut self, nodes: &[GraphNode]) {
        self.query.pop();
        self.rerank(nodes);
    }

    pub fn next(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.matches.len();
    }

    pub fn previous(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = (self.selected + self.matches.len() - 1) % self.matches.len();
    }

    fn rerank(&mut self, nodes: &[GraphNode]) {
        self.matches = rank(nodes, &self.query);
        // A changed query is a different result list, so the old highlight
        // index means nothing in it; the best match is the useful default.
        self.selected = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::commit::CommitInfo;
    use crate::git::graph::{CellType, GraphNode};

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

    #[test]
    fn an_empty_query_matches_nothing() {
        let nodes = vec![node("fix the thing", "ann")];
        assert!(rank(&nodes, "").is_empty());
    }

    #[test]
    fn a_subsequence_of_the_summary_matches() {
        let nodes = vec![
            node("fix the parser", "ann"),
            node("add a renderer", "bo"),
            node("unrelated", "cy"),
        ];
        let hits = rank(&nodes, "fxprs");
        assert_eq!(
            hits,
            vec![0],
            "fuzzy subsequence should find the parser fix"
        );
    }

    #[test]
    fn the_author_is_searched_too() {
        let nodes = vec![node("something", "ann"), node("other", "bo")];
        let hits = rank(&nodes, "ann");
        assert_eq!(hits, vec![0]);
    }

    #[test]
    fn better_matches_rank_first() {
        let nodes = vec![node("a p a r s e r somewhere", "x"), node("parser", "y")];
        let hits = rank(&nodes, "parser");
        assert_eq!(
            hits.first(),
            Some(&1),
            "the exact word should outrank the scattered one"
        );
    }

    #[test]
    fn connector_and_uncommitted_rows_are_never_matched() {
        let mut connector = node("ignored", "x");
        connector.commit = None;
        let mut uncommitted = node("ignored", "x");
        uncommitted.commit = None;
        uncommitted.uncommitted = Some(3);
        let nodes = vec![connector, uncommitted, node("real commit", "x")];
        assert_eq!(rank(&nodes, "commit"), vec![2]);
    }

    #[test]
    fn editing_the_query_moves_through_matches() {
        let nodes = vec![node("alpha", "x"), node("alphabet", "y")];
        let mut s = Search::default();
        for c in "alpha".chars() {
            s.push(c, &nodes);
        }
        assert_eq!(s.matches().len(), 2);
        assert_eq!(s.selected(), Some(0));
        s.next();
        assert_eq!(s.selected(), Some(1));
        s.next();
        assert_eq!(s.selected(), Some(0), "next wraps");
        s.previous();
        assert_eq!(s.selected(), Some(1), "previous wraps the other way");

        s.backspace(&nodes);
        assert_eq!(s.query(), "alph");
    }

    /// `editing_the_query_moves_through_matches` cannot tell `selected` from
    /// `selected_row` apart: its two matches happen to be rows 0 and 1, so
    /// both readings give the same numbers. This one separates them — the
    /// matching rows are 1 and 3, and only the row-index reading can produce
    /// those.
    #[test]
    fn selected_indexes_the_match_list_and_selected_row_indexes_the_graph() {
        let nodes = vec![
            node("nothing here", "x"),
            node("alpha", "x"),
            node("nothing here either", "x"),
            node("alphabet", "y"),
        ];
        let mut s = Search::default();
        for c in "alpha".chars() {
            s.push(c, &nodes);
        }
        assert_eq!(s.matches(), &[1, 3], "matches are graph row indices");
        assert_eq!(s.selected(), Some(0), "the first entry of the dropdown");
        assert_eq!(s.selected_row(), Some(1), "which is graph row 1");
        s.next();
        assert_eq!(s.selected(), Some(1));
        assert_eq!(s.selected_row(), Some(3));
    }

    /// Nothing matched is not the same as nothing typed, and neither may hand
    /// the caller a row to jump to.
    #[test]
    fn no_matches_leaves_nothing_selected() {
        let nodes = vec![node("alpha", "x")];
        let mut s = Search::default();
        assert_eq!(s.selected(), None, "an empty query selects nothing");
        assert_eq!(s.selected_row(), None);
        for c in "zzzz".chars() {
            s.push(c, &nodes);
        }
        assert!(s.matches().is_empty());
        assert_eq!(s.selected(), None);
        assert_eq!(s.selected_row(), None);
        // Movement on an empty list must not wrap into a phantom entry.
        s.next();
        s.previous();
        assert_eq!(s.selected(), None);
    }
}
