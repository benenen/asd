//! The overlay's own state: what is loaded, what is selected, what is on
//! screen, and how a key changes those.

use std::collections::HashMap;
use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Widget};

use crate::git::commit::CommitInfo;
use crate::git::graph::GraphBuilder;
use crate::git::refs::RefInfo;
use crate::git::repo::{OpenError, Repo};
use crate::ui::graph_view::draw_rows;

/// Rows fed into the *layout* before the first frame. Large enough to fill any
/// terminal several times over, small enough that the first layout is cheap.
///
/// It bounds the layout only, never the read: by the time it is used `reload`
/// has already drained the entire history walk, so opening costs O(whole
/// repository) whatever this number is. Lowering it does not make opening
/// faster.
pub const PAGE_FIRST: usize = 500;
/// Rows added each time the selection approaches the loaded tail.
pub const PAGE_MORE: usize = 2000;
/// How close the selection may get to the tail before more is loaded.
const PAGE_MARGIN: usize = 200;

/// What `asd-tui` should do after handing over an event.
///
/// `#[non_exhaustive]`: phases 2 and 3 add outcomes for the write dialogs, and
/// a host that matches every variant today must keep compiling then. Hosts
/// need a fallback arm; treating an unknown outcome as "handled, repaint" is
/// the safe reading.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// Handled. The host keeps the overlay open and repaints: every
    /// navigation key returns this, so treating it as "nothing changed"
    /// would leave the selection visibly stuck. Nothing in phase 1
    /// constructs `Redraw`, so `Consumed` is the repaint signal.
    Consumed,
    /// Handled and the frame needs repainting. Reserved for phase 2's diff
    /// worker; no phase 1 path returns it, so a host must not wait for it.
    Redraw,
    /// Close the overlay.
    Dismiss,
    /// Put this on the clipboard. `asd-tui` owns the OSC 52 path, so this
    /// crate hands the text back rather than emitting the sequence itself.
    Copy(String),
}

/// The overlay: a repository, the rows loaded from it, and the view state.
#[derive(Debug)]
pub struct GitGraph {
    repo: Repo,
    builder: GraphBuilder,
    /// Commits pulled from the walk but not yet fed, so a page boundary in the
    /// middle of the iterator does not lose one.
    pending: std::vec::IntoIter<CommitInfo>,
    exhausted: bool,
    decorations: HashMap<gix::ObjectId, Vec<RefInfo>>,
    selected: usize,
    /// Index of the first row drawn; follows the selection.
    first_row: usize,
    /// Rows the last frame had room for, so paging keys know their step.
    viewport_rows: usize,
    error: Option<String>,
}

impl GitGraph {
    /// Open the repository containing `path` and load the first page.
    pub fn open(path: &Path) -> Result<Self, OpenError> {
        let repo = Repo::open(path)?;
        let decorations = repo.refs().map(group_refs).unwrap_or_default();
        let mut me = Self {
            repo,
            builder: GraphBuilder::new(),
            pending: Vec::new().into_iter(),
            exhausted: false,
            decorations,
            selected: 0,
            first_row: 0,
            viewport_rows: 1,
            error: None,
        };
        me.reload();
        me.load_more(PAGE_FIRST);
        Ok(me)
    }

    pub fn workdir(&self) -> &Path {
        self.repo.workdir()
    }

    pub fn row_count(&self) -> usize {
        self.builder.nodes().len()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Phase 1 has no background work. The method exists now so `asd-tui`'s
    /// event loop is wired for the diff worker phase 2 adds.
    pub fn poll(&mut self) -> bool {
        false
    }

    /// Re-read the whole history from scratch.
    fn reload(&mut self) {
        self.builder = GraphBuilder::new();
        self.exhausted = false;
        self.error = None;
        match self.repo.walk() {
            Ok(walk) => {
                // The walk is drained up front, deliberately: `rev_walk`
                // borrows the repository, so the iterator cannot be parked in
                // a struct that also owns it. Only the *layout* had to be
                // incremental — rebuilding that per page is what would have
                // been quadratic. Draining is affordable on measurement:
                // 14 500 commits walk in 152 ms, inside the 300 ms open budget.
                let taken: Vec<_> = walk.collect();
                let mut commits = Vec::with_capacity(taken.len());
                for item in taken {
                    match item {
                        Ok(c) => commits.push(c),
                        Err(e) => {
                            self.error = Some(e.to_string());
                            break;
                        }
                    }
                }
                self.pending = commits.into_iter();
            }
            Err(e) => {
                self.error = Some(e.to_string());
                self.exhausted = true;
            }
        }
    }

    /// Feed up to `count` more commits into the layout.
    fn load_more(&mut self, count: usize) {
        if self.exhausted {
            return;
        }
        for _ in 0..count {
            match self.pending.next() {
                Some(c) => self.builder.feed(c),
                None => {
                    self.exhausted = true;
                    break;
                }
            }
        }
    }

    /// Load more when the selection is within `PAGE_MARGIN` of the tail.
    fn ensure_loaded_around(&mut self, index: usize) {
        while !self.exhausted && index + PAGE_MARGIN >= self.row_count() {
            self.load_more(PAGE_MORE);
        }
    }

    fn select(&mut self, index: usize) -> Outcome {
        self.ensure_loaded_around(index);
        let last = self.row_count().saturating_sub(1);
        self.selected = index.min(last);
        // Keep the selection inside the viewport.
        if self.selected < self.first_row {
            self.first_row = self.selected;
        } else if self.selected >= self.first_row + self.viewport_rows {
            self.first_row = self.selected + 1 - self.viewport_rows;
        }
        Outcome::Consumed
    }

    /// The commit on the selected row, if that row is a commit rather than a
    /// connector.
    fn selected_commit(&self) -> Option<&CommitInfo> {
        self.builder
            .nodes()
            .get(self.selected)
            .and_then(|n| n.commit.as_ref())
    }

    /// Handle one key.
    ///
    /// This ignores `KeyEvent::kind`: a host that forwards `Release` as well
    /// as `Press` — which crossterm does emit once the kitty keyboard
    /// protocol is enabled — moves the selection twice per keypress.
    /// Filtering `KeyEventKind::Release` out is the caller's job. `Repeat`
    /// must *not* be filtered: it is what makes holding `j` down keep
    /// scrolling.
    pub fn on_key(&mut self, key: KeyEvent) -> Outcome {
        let page = self.viewport_rows.max(1);
        // A half page still has to be at least one row: `viewport_rows` is 1
        // before the first frame and can be 1 in a three-row-tall overlay,
        // and `1 / 2` would make these keys permanently dead rather than
        // merely small-stepped.
        let half_page = (page / 2).max(1);
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down) => {
                self.select(self.selected.saturating_add(1))
            }
            (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up) => {
                self.select(self.selected.saturating_sub(1))
            }
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                self.select(self.selected.saturating_add(half_page))
            }
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                self.select(self.selected.saturating_sub(half_page))
            }
            (KeyModifiers::NONE, KeyCode::PageDown) => {
                self.select(self.selected.saturating_add(page))
            }
            (KeyModifiers::NONE, KeyCode::PageUp) => {
                self.select(self.selected.saturating_sub(page))
            }
            (KeyModifiers::NONE, KeyCode::Char('g') | KeyCode::Home) => self.select(0),
            (KeyModifiers::SHIFT, KeyCode::Char('G')) | (_, KeyCode::End) => {
                // Jumping to the bottom means the bottom of the whole history,
                // not the bottom of what happens to be loaded.
                while !self.exhausted {
                    self.load_more(PAGE_MORE);
                }
                self.select(self.row_count().saturating_sub(1))
            }
            (_, KeyCode::Char('@')) => {
                let head = self.repo.head();
                let target = head.and_then(|h| {
                    self.builder
                        .nodes()
                        .iter()
                        .position(|n| n.commit.as_ref().is_some_and(|c| c.id == h))
                });
                match target {
                    Some(i) => self.select(i),
                    None => Outcome::Consumed,
                }
            }
            (KeyModifiers::SHIFT, KeyCode::Char('R')) => {
                let keep = self.selected;
                self.reload();
                self.load_more(PAGE_FIRST);
                self.decorations = self.repo.refs().map(group_refs).unwrap_or_default();
                self.select(keep)
            }
            (KeyModifiers::NONE, KeyCode::Char('y')) => match self.selected_commit() {
                Some(c) => Outcome::Copy(c.id.to_string()),
                None => Outcome::Consumed,
            },
            (_, KeyCode::Char('q') | KeyCode::Esc) => Outcome::Dismiss,
            _ => Outcome::Consumed,
        }
    }

    pub fn on_mouse(&mut self, ev: MouseEvent) -> Outcome {
        match ev.kind {
            MouseEventKind::ScrollDown => self.select(self.selected.saturating_add(3)),
            MouseEventKind::ScrollUp => self.select(self.selected.saturating_sub(3)),
            _ => Outcome::Consumed,
        }
    }
}

/// Index decorations by the commit they label.
fn group_refs(refs: Vec<RefInfo>) -> HashMap<gix::ObjectId, Vec<RefInfo>> {
    let mut out: HashMap<gix::ObjectId, Vec<RefInfo>> = HashMap::new();
    for r in refs {
        out.entry(r.target).or_default().push(r);
    }
    out
}

impl Widget for &mut GitGraph {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Clamp to the buffer before anything derives a sub-area from it.
        // `Block::render` intersects internally, but `Block::inner` does not,
        // so an `area` running past the buffer's edge would hand `draw_rows`
        // and `draw_message` an out-of-bounds region and panic on `asd ui`'s
        // main thread — blanking every session the user has open. Containment
        // is this crate's own guarantee, not a precondition on its callers.
        let area = area.intersection(buf.area);

        let title = format!(" Git Graph — {} ", self.repo.workdir().display());
        let mut block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::Rgb(0x8B, 0x94, 0xA2)));
        // A partial read keeps the rows it managed to read: one unreadable
        // commit must not hide an otherwise fine history, so the failure is
        // surfaced beside the graph rather than replacing it. It goes on the
        // bottom border, not the top one, because the top title already
        // carries the workdir path — a repository nested a few directories
        // deep truncates anything appended after it clean off the border.
        // With no rows at all there is nothing to hide, and the message
        // becomes the whole body instead (below).
        if let (Some(error), true) = (self.error.as_deref(), self.row_count() > 0) {
            block = block.title_bottom(Line::styled(
                format!(" partial read: {error} "),
                Style::default().fg(Color::Rgb(0xD8, 0x6C, 0x6C)),
            ));
        }
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        // Remember the viewport so paging keys know their step next time.
        self.viewport_rows = inner.height as usize;

        if self.row_count() == 0 {
            let msg = match self.error.as_deref() {
                Some(error) => format!("cannot read this repository: {error}"),
                None => "no commits yet".to_string(),
            };
            crate::ui::graph_view::draw_message(buf, inner, &msg);
            return;
        }
        draw_rows(
            buf,
            inner,
            self.builder.nodes(),
            &self.decorations,
            self.first_row,
            self.selected,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::Fixture;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Every symbol in `area`, row by row, as one string.
    fn buffer_text(buf: &Buffer, area: Rect) -> String {
        (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect()
    }

    fn graph_with(n: usize, tag: &str) -> (Fixture, GitGraph) {
        let fx = Fixture::new(tag);
        for i in 0..n {
            fx.commit(&format!("commit {i}"));
        }
        let g = GitGraph::open(fx.path()).expect("fixture opens");
        (fx, g)
    }

    #[test]
    fn opens_with_the_newest_commit_selected() {
        let (_fx, g) = graph_with(3, "state-open");
        assert_eq!(g.selected(), 0);
        assert_eq!(g.row_count(), 3);
    }

    #[test]
    fn j_and_k_move_the_selection_and_stop_at_the_ends() {
        let (_fx, mut g) = graph_with(3, "state-jk");
        assert!(matches!(
            g.on_key(key(KeyCode::Char('k'))),
            Outcome::Consumed
        ));
        assert_eq!(g.selected(), 0, "k at the top stays at the top");

        g.on_key(key(KeyCode::Char('j')));
        assert_eq!(g.selected(), 1);
        g.on_key(key(KeyCode::Char('j')));
        g.on_key(key(KeyCode::Char('j')));
        g.on_key(key(KeyCode::Char('j')));
        assert_eq!(g.selected(), 2, "j at the bottom stays at the bottom");
    }

    #[test]
    fn g_and_shift_g_jump_to_the_ends() {
        let (_fx, mut g) = graph_with(5, "state-gg");
        g.on_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(g.selected(), g.row_count() - 1);
        g.on_key(key(KeyCode::Char('g')));
        assert_eq!(g.selected(), 0);
    }

    #[test]
    fn q_and_esc_dismiss() {
        let (_fx, mut g) = graph_with(2, "state-quit");
        assert!(matches!(
            g.on_key(key(KeyCode::Char('q'))),
            Outcome::Dismiss
        ));
        assert!(matches!(g.on_key(key(KeyCode::Esc)), Outcome::Dismiss));
    }

    #[test]
    fn y_hands_the_hash_back_to_the_host() {
        // asd-tui owns the OSC 52 path; this crate must not emit it itself.
        let (_fx, mut g) = graph_with(2, "state-copy");
        let Outcome::Copy(text) = g.on_key(key(KeyCode::Char('y'))) else {
            panic!("y must yield Outcome::Copy");
        };
        assert_eq!(
            text.len(),
            40,
            "a full sha1, not the abbreviation: {text:?}"
        );
    }

    #[test]
    fn an_empty_repository_opens_with_no_rows() {
        let fx = Fixture::new("state-empty");
        let g = GitGraph::open(fx.path()).expect("an unborn repository still opens");
        assert_eq!(g.row_count(), 0);
        // Moving around an empty graph must not panic.
        let mut g = g;
        g.on_key(key(KeyCode::Char('j')));
        g.on_key(key(KeyCode::Char('G')));
        // Crossterm reports an uppercase letter with SHIFT set, so the line
        // above lands on the fallback arm rather than the jump-to-bottom one.
        // Send the shape a real terminal sends, so an empty graph actually
        // walks the `G` handler and its load-everything loop.
        g.on_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(g.selected(), 0);
    }

    #[test]
    fn poll_reports_no_work_in_phase_one() {
        let (_fx, mut g) = graph_with(1, "state-poll");
        assert!(!g.poll(), "no background work exists yet");
    }

    /// `Widget::render` is the call site Task 7's panic-safety argument rests
    /// on: `draw_rows` and `draw_message` are only sound while the area handed
    /// to them is contained in the buffer, and `block.inner` is what shrinks
    /// the widget's area to one that still is. A border eats two columns and
    /// two rows, so every area under 3x3 has a degenerate inner area — exactly
    /// where the early return has to hold. This runs on `asd ui`'s main
    /// thread, where a panic blanks every session the user has open, so the
    /// sweep covers non-zero origins (the overlay is drawn inset in
    /// production) and all three states the widget can be in: rows, no rows,
    /// and a read error.
    ///
    /// The only assertion is that none of this panics.
    #[test]
    fn rendering_at_any_size_does_not_panic() {
        let (_fx, mut with_rows) = graph_with(3, "state-render");
        let empty_fx = Fixture::new("state-render-empty");
        let mut no_rows = GitGraph::open(empty_fx.path()).expect("an unborn repository opens");
        let (_err_fx, mut errored) = graph_with(1, "state-render-error");
        errored.error = Some("simulated read failure with a long message".to_string());

        for graph in [&mut with_rows, &mut no_rows, &mut errored] {
            for &ox in &[0u16, 2] {
                for &oy in &[0u16, 1] {
                    for width in 0..=10u16 {
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
                            (&mut *graph).render(area, &mut buf);
                        }
                    }
                }
            }

            // The precondition violation itself, now that handling it is the
            // contract rather than a demand on the caller: an area running
            // past the buffer's edge, and one starting outside it entirely.
            // `Block::render` self-defends, but `Block::inner` does not, so
            // without the clamp in `render` these index out of bounds.
            for &(bx, by, bw, bh) in &[
                (0u16, 0u16, 0u16, 0u16),
                (0, 0, 1, 1),
                (0, 0, 5, 3),
                (0, 0, 20, 8),
                // A buffer that does not start at the origin: a clamp written
                // as `min(width)` rather than a real intersection passes every
                // case above and fails this one.
                (4, 2, 12, 6),
            ] {
                let mut buf = Buffer::empty(Rect::new(bx, by, bw, bh));
                for &area in &[
                    Rect::new(0, 0, 200, 60),
                    Rect::new(bx, by, 200, 60),
                    Rect::new(bx.saturating_add(bw), by.saturating_add(bh), 40, 20),
                    Rect::new(180, 50, 40, 20),
                    Rect::new(u16::MAX - 2, u16::MAX - 2, 8, 8),
                ] {
                    (&mut *graph).render(area, &mut buf);
                }
            }
        }
    }

    #[test]
    fn a_half_page_key_moves_even_when_the_viewport_is_one_row() {
        // `viewport_rows` is 1 until the first frame, and stays 1 in a
        // three-row-tall overlay. A raw `page / 2` is 0 there, which makes
        // Ctrl-D and Ctrl-U permanently dead rather than merely small-stepped.
        let (_fx, mut g) = graph_with(3, "state-halfpage");
        assert_eq!(g.viewport_rows, 1, "no frame has been rendered yet");

        g.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(g.selected(), 1, "Ctrl-D must move at least one row");
        g.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(g.selected(), 0, "Ctrl-U must move at least one row");
    }

    #[test]
    fn a_partial_read_error_does_not_hide_the_rows_it_did_read() {
        // A mid-walk failure keeps the commits already drained. Showing the
        // error *instead of* them would blank an otherwise fine history over
        // one unreadable commit, so it belongs in the title, not the body.
        let (_fx, mut g) = graph_with(3, "state-partial");
        g.error = Some("reading a commit: object not found".to_string());

        let area = Rect::new(0, 0, 60, 6);
        let mut buf = Buffer::empty(area);
        (&mut g).render(area, &mut buf);
        let text = buffer_text(&buf, area);

        assert!(
            text.contains("commit 2"),
            "the rows survive the error: {text:?}"
        );
        assert!(
            text.contains("partial read"),
            "the error is still surfaced: {text:?}"
        );
        assert!(
            !text.contains("cannot read this repository"),
            "the whole-body failure message is for an empty graph only: {text:?}"
        );
    }

    #[test]
    fn a_read_error_with_no_rows_is_the_whole_message() {
        let fx = Fixture::new("state-error-empty");
        let mut g = GitGraph::open(fx.path()).expect("an unborn repository opens");
        g.error = Some("opening references: broken".to_string());

        let area = Rect::new(0, 0, 60, 6);
        let mut buf = Buffer::empty(area);
        (&mut g).render(area, &mut buf);
        let text = buffer_text(&buf, area);
        assert!(text.contains("cannot read this repository"), "{text:?}");
    }

    #[test]
    fn an_empty_repository_renders_its_own_message() {
        let fx = Fixture::new("state-render-message");
        let mut g = GitGraph::open(fx.path()).expect("an unborn repository opens");
        let area = Rect::new(0, 0, 40, 5);
        let mut buf = Buffer::empty(area);
        (&mut g).render(area, &mut buf);

        let text = buffer_text(&buf, area);
        assert!(text.contains("no commits yet"), "{text:?}");
    }

    #[test]
    fn scrolling_past_the_loaded_tail_loads_more() {
        // The first page is PAGE_FIRST rows; walking past it must extend
        // rather than stop.
        let (_fx, mut g) = graph_with(PAGE_FIRST + 40, "state-page");
        assert_eq!(g.row_count(), PAGE_FIRST, "first page is bounded");
        g.on_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert!(
            g.row_count() > PAGE_FIRST,
            "G loads the rest: {} rows",
            g.row_count()
        );
        assert_eq!(g.selected(), g.row_count() - 1);
    }
}
