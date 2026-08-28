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
use crate::ui::layout::{LayoutMap, Pane};

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
/// Rows one wheel notch moves.
const WHEEL_ROWS: isize = 3;
/// Shown where a diff would be when the worker thread is gone. The graph still
/// works without it, so this is a message rather than a failure of the overlay.
const WORKER_GONE: &str = "diffs are unavailable";

/// What `asd-tui` should do after handing over an event.
///
/// `#[non_exhaustive]`: phases 2 and 3 add outcomes for the write dialogs, and
/// a host that matches every variant today must keep compiling then. Hosts
/// need a fallback arm; treating an unknown outcome as "handled, repaint" is
/// the safe reading.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// Handled. The host keeps the overlay open. After a key it repaints —
    /// every navigation key returns this, so treating it as "nothing changed"
    /// would leave the selection visibly stuck. After a *mouse* event the
    /// host repaints only when `selected()` moved, because pointer motion is
    /// reported continuously and repainting per report would redraw the whole
    /// overlay on every pixel of movement.
    Consumed,
    /// Handled, and something changed that the host cannot see by comparing
    /// `selected()`: focus moved between panes, or a pane other than the
    /// graph scrolled. The frame must be repainted.
    Redraw,
    /// Close the overlay.
    Dismiss,
    /// Put this on the clipboard. `asd-tui` owns the OSC 52 path, so this
    /// crate hands the text back rather than emitting the sequence itself.
    Copy(String),
}

/// What the detail pane has for the selected commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailState {
    /// A request is outstanding.
    Loading,
    Ready(crate::git::diff::CommitDiff),
    /// The diff failed; the message is shown in the pane.
    Failed(String),
    /// The worker thread is gone. The graph still works.
    Unavailable,
}

/// Which layer of the overlay is on top.
///
/// The layers are a stack, not a set of independent flags: `q`/`Esc` pops one
/// layer, and only the bottom one closes the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The three panes.
    Normal,
    /// One file's diff, filling the overlay.
    FileDiff,
}

/// What the file diff view has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileDiffState {
    /// No file has been opened yet. Only reachable in [`Mode::Normal`], where
    /// nothing draws it.
    Closed,
    /// A request for this path is outstanding.
    Loading(String),
    /// The diff, with its lines already highlighted on the worker thread.
    Ready(crate::worker::HighlightedDiff),
    Failed(String),
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
    /// Computes diffs off the render thread. `None` when it failed to start.
    worker: Option<crate::worker::DiffWorker>,
    detail: DetailState,
    /// Which commit the outstanding request (if any) is for. A reply whose id
    /// does not match this is stale and must be discarded.
    detail_for: Option<gix::ObjectId>,
    /// The three panes' rectangles from the last frame, so a mouse event can
    /// be routed to the pane it landed in. Empty until the first render.
    layout: LayoutMap,
    /// Lines scrolled past in the detail pane.
    detail_scroll: usize,
    /// Rows the detail pane had to show in the last frame, so scrolling it
    /// can stop at the end instead of running off into blank space. Zero
    /// until the first frame, and whenever the area is too short to give the
    /// detail pane any height at all.
    detail_rows: usize,
    /// Which pane has keyboard focus. `Tab` moves it; it decides which pane
    /// `j`/`k` act on and which border is tinted.
    focus: Pane,
    /// Which row is selected in the changed-files pane.
    file_selected: usize,
    /// Rows scrolled past in the changed-files pane.
    file_scroll: usize,
    /// Which layer is on top: the three panes, or one file's diff.
    mode: Mode,
    /// The file diff view's content. Kept after the view is closed so
    /// reopening the same file is instant.
    file_diff: FileDiffState,
    /// Which commit and path the outstanding file request is for. A reply for
    /// anything else is stale and must be discarded, exactly as for `detail`:
    /// there is no cancellation, so a request for a file the user has already
    /// navigated away from still comes back.
    file_diff_for: Option<(gix::ObjectId, String)>,
    /// Lines scrolled past in the file diff view.
    file_diff_scroll: usize,
    /// Rows the file diff view had room for in the last frame, so scrolling it
    /// can stop at the end of the diff. Zero until the first frame.
    file_diff_rows: usize,
}

impl GitGraph {
    /// Open the repository containing `path` and load the first page.
    pub fn open(path: &Path) -> Result<Self, OpenError> {
        let repo = Repo::open(path)?;
        let decorations = repo.refs().map(group_refs).unwrap_or_default();
        // A worker that cannot start leaves detail as `Unavailable` rather
        // than failing the whole overlay: the graph is still useful without
        // diffs.
        let worker = crate::worker::DiffWorker::new(path).ok();
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
            worker,
            detail: DetailState::Unavailable,
            detail_for: None,
            layout: LayoutMap::default(),
            detail_scroll: 0,
            detail_rows: 0,
            focus: Pane::Graph,
            file_selected: 0,
            file_scroll: 0,
            mode: Mode::Normal,
            file_diff: FileDiffState::Closed,
            file_diff_for: None,
            file_diff_scroll: 0,
            file_diff_rows: 0,
        };
        me.reload();
        me.load_more(PAGE_FIRST);
        me.request_detail();
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

    /// Which pane the keyboard is aimed at.
    pub fn focus(&self) -> Pane {
        self.focus
    }

    /// Which row of the changed-files pane is selected.
    pub fn file_selected(&self) -> usize {
        self.file_selected
    }

    /// Which layer of the overlay is on top.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// What the file diff view has. Meaningful while `mode` is
    /// [`Mode::FileDiff`]; otherwise it is whatever was last opened.
    pub fn file_diff(&self) -> &FileDiffState {
        &self.file_diff
    }

    #[cfg(test)]
    pub(crate) fn layout_for_test(&self) -> LayoutMap {
        self.layout
    }

    /// Take everything the worker finished. Returns true when the frame needs
    /// repainting.
    ///
    /// Replies are applied before the worker's aliveness is acted on. There
    /// is no cancellation, so a request always completes; if the thread dies
    /// right after sending its last reply, that reply is still sitting in
    /// the channel when `drain` takes it, and it must be shown rather than
    /// thrown away in favour of a bare "unavailable". Aliveness is only a
    /// backstop for the request that death left with no answer at all: if
    /// nothing resolved the outstanding request, `detail` is still `Loading`
    /// after the loop below, and a `Loading` that will now never resolve is
    /// exactly when the fallback belongs.
    pub fn poll(&mut self) -> bool {
        let Some(worker) = self.worker.as_mut() else {
            return false;
        };
        let replies = worker.drain();
        let alive = worker.is_alive();
        let mut dirty = false;
        for reply in replies {
            dirty |= self.accept_reply(reply);
        }
        if !alive && matches!(self.detail, DetailState::Loading) {
            self.detail = DetailState::Unavailable;
            dirty = true;
        }
        // The same backstop for the file view: a `Loading` that will never
        // resolve would otherwise spin forever behind a dead worker.
        if !alive && matches!(self.file_diff, FileDiffState::Loading(_)) {
            self.file_diff = FileDiffState::Failed(WORKER_GONE.to_string());
            self.file_diff_for = None;
            dirty = true;
        }
        dirty
    }

    /// Apply one reply. A reply for a commit that is no longer selected is
    /// dropped: on a large repository the selection moves faster than the
    /// worker, and a late answer must not overwrite the current row's detail.
    fn accept_reply(&mut self, reply: crate::worker::Reply) -> bool {
        match reply {
            crate::worker::Reply::Commit { id, result } => {
                if self.detail_for != Some(id) {
                    return false;
                }
                self.detail = match result {
                    Ok(diff) => DetailState::Ready(diff),
                    Err(msg) => DetailState::Failed(msg),
                };
                // A new commit's file list must start at its first file, not
                // wherever the previous commit's list happened to leave off.
                if matches!(self.detail, DetailState::Ready(_)) {
                    self.file_selected = 0;
                    self.file_scroll = 0;
                }
                true
            }
            crate::worker::Reply::File {
                commit,
                path,
                result,
            } => {
                // Same staleness rule as the commit arm, on the pair that
                // identifies the request: the user can close the view, move to
                // another commit and open the same path again before the first
                // answer lands.
                if self.file_diff_for.as_ref() != Some(&(commit, path)) {
                    return false;
                }
                self.file_diff = match result {
                    Ok(highlighted) => FileDiffState::Ready(highlighted),
                    Err(msg) => FileDiffState::Failed(msg),
                };
                true
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn accept_reply_for_test(&mut self, reply: crate::worker::Reply) -> bool {
        self.accept_reply(reply)
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
        let next = index.min(last);
        if next != self.selected {
            // The lower panes describe the selected commit, so a different
            // commit starts them at the top. The detail pane in particular
            // changes the moment the selection does — before any reply — so
            // leaving its offset alone would show the new commit already
            // scrolled. `accept_reply` resets the file list again when the
            // reply lands, which is the point at which the list it indexes
            // into actually exists.
            self.detail_scroll = 0;
            self.file_selected = 0;
            self.file_scroll = 0;
        }
        self.selected = next;
        // Keep the selection inside the viewport.
        if self.selected < self.first_row {
            self.first_row = self.selected;
        } else if self.selected >= self.first_row + self.viewport_rows {
            self.first_row = self.selected + 1 - self.viewport_rows;
        }
        self.request_detail();
        Outcome::Consumed
    }

    /// How many rows `pane`'s list has.
    fn pane_len(&self, pane: Pane) -> usize {
        match pane {
            Pane::Graph => self.row_count(),
            Pane::Files => match &self.detail {
                DetailState::Ready(d) => d.files.len(),
                _ => 0,
            },
            // The detail pane scrolls by line and has no selection, so it has
            // no list length; `move_pane` clamps it against `detail_rows`.
            Pane::Detail => 0,
        }
    }

    /// Move `pane`'s selection or scroll by `delta` rows.
    ///
    /// The pane is an argument rather than being read from `self.focus`
    /// because the wheel scrolls the pane under the pointer, not the focused
    /// one. Passing it in means the focus is never swapped and so cannot be
    /// left somewhere the user did not put it — including on the paths that
    /// return early because the movement was clamped away.
    ///
    /// `Redraw` rather than `Consumed` where something moved that is not the
    /// graph selection: the host detects a moved graph selection itself, but
    /// cannot see a scrolled detail pane, so a wheel over one would otherwise
    /// change the state without repainting it.
    fn move_pane(&mut self, pane: Pane, delta: isize) -> Outcome {
        match pane {
            Pane::Graph => {
                let next = if delta < 0 {
                    self.selected.saturating_sub(delta.unsigned_abs())
                } else {
                    self.selected.saturating_add(delta as usize)
                };
                self.select(next)
            }
            Pane::Files => {
                let last = self.pane_len(Pane::Files).saturating_sub(1);
                let next = if delta < 0 {
                    self.file_selected.saturating_sub(delta.unsigned_abs())
                } else {
                    self.file_selected.saturating_add(delta as usize)
                }
                .min(last);
                if next == self.file_selected {
                    return Outcome::Consumed;
                }
                self.file_selected = next;
                self.keep_file_visible();
                Outcome::Redraw
            }
            Pane::Detail => {
                // One line stays on screen at the bottom: scrolling into
                // blank space would need as many keys to come back from as it
                // took to get there, and would repaint the overlay for each.
                let visible = (self.layout.detail.height.saturating_sub(2) as usize).max(1);
                let last = self.detail_rows.saturating_sub(visible);
                let next = if delta < 0 {
                    self.detail_scroll.saturating_sub(delta.unsigned_abs())
                } else {
                    self.detail_scroll.saturating_add(delta as usize)
                }
                .min(last);
                if next == self.detail_scroll {
                    return Outcome::Consumed;
                }
                self.detail_scroll = next;
                Outcome::Redraw
            }
        }
    }

    /// Move the focused pane, which is what the keyboard acts on.
    fn move_focused(&mut self, delta: isize) -> Outcome {
        self.move_pane(self.focus, delta)
    }

    /// Keep the file selection inside the changed-files pane's viewport.
    ///
    /// The height comes from the last frame's layout: before the first frame
    /// there is no viewport to scroll within, and leaving the offset alone
    /// there is right, because nothing has been shown yet.
    fn keep_file_visible(&mut self) {
        if self.file_selected < self.file_scroll {
            self.file_scroll = self.file_selected;
            return;
        }
        // The pane draws inside its border, so two of its rows are not list
        // rows.
        let visible = self.layout.files.height.saturating_sub(2) as usize;
        if visible > 0 && self.file_selected >= self.file_scroll.saturating_add(visible) {
            self.file_scroll = self.file_selected + 1 - visible;
        }
    }

    /// The commit on the selected row, if that row is a commit rather than a
    /// connector.
    fn selected_commit(&self) -> Option<&CommitInfo> {
        self.builder
            .nodes()
            .get(self.selected)
            .and_then(|n| n.commit.as_ref())
    }

    /// The id of the selected row's commit, if that row is a commit rather
    /// than a connector.
    pub fn selected_id(&self) -> Option<gix::ObjectId> {
        self.selected_commit().map(|c| c.id)
    }

    pub fn detail(&self) -> &DetailState {
        &self.detail
    }

    /// Ask the worker for the selected commit's diff. Called whenever the
    /// selection lands on a different commit.
    fn request_detail(&mut self) {
        let Some(id) = self.selected_id() else {
            self.detail = DetailState::Ready(Default::default());
            self.detail_for = None;
            return;
        };
        if self.detail_for == Some(id) {
            return; // Already asked for exactly this.
        }
        match self.worker.as_mut() {
            Some(w) if w.is_alive() => {
                w.request(crate::worker::Request::Commit(id));
                self.detail_for = Some(id);
                self.detail = DetailState::Loading;
            }
            _ => {
                self.detail = DetailState::Unavailable;
                self.detail_for = None;
            }
        }
    }

    /// Handle one key.
    ///
    /// The movement keys act on the focused pane, which `Tab` cycles. The
    /// keys that are about a *commit* rather than about a list — `g`, `G`,
    /// `@`, `y`, `R` — stay on the graph whatever has focus: they have no
    /// meaning in the two panes that describe the commit the graph selected.
    ///
    /// This ignores `KeyEvent::kind`: a host that forwards `Release` as well
    /// as `Press` — which crossterm does emit once the kitty keyboard
    /// protocol is enabled — moves the selection twice per keypress.
    /// Filtering `KeyEventKind::Release` out is the caller's job. `Repeat`
    /// must *not* be filtered: it is what makes holding `j` down keep
    /// scrolling.
    pub fn on_key(&mut self, key: KeyEvent) -> Outcome {
        // The file diff view is a layer on top, not a fourth pane: while it is
        // open every key belongs to it. Falling through to the pane keys would
        // move the changed-files selection under a view the user cannot see it
        // in, so closing the view would land them on a different file.
        if self.mode == Mode::FileDiff {
            return self.file_diff_key(key);
        }
        let page = self.viewport_rows.max(1);
        // A half page still has to be at least one row: `viewport_rows` is 1
        // before the first frame and can be 1 in a three-row-tall overlay,
        // and `1 / 2` would make these keys permanently dead rather than
        // merely small-stepped.
        let half_page = (page / 2).max(1);
        // Both step counts come from a `u16` pane height, so neither can be
        // large enough for the `isize` conversion to wrap.
        let page = page as isize;
        let half_page = half_page as isize;
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.focus = match self.focus {
                    Pane::Graph => Pane::Detail,
                    Pane::Detail => Pane::Files,
                    Pane::Files => Pane::Graph,
                };
                Outcome::Redraw
            }
            // Crossterm reports Shift+Tab as `BackTab`, and with `SHIFT` set
            // on most terminals but not all, so the modifier is not matched.
            (_, KeyCode::BackTab) => {
                self.focus = match self.focus {
                    Pane::Graph => Pane::Files,
                    Pane::Files => Pane::Detail,
                    Pane::Detail => Pane::Graph,
                };
                Outcome::Redraw
            }
            (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down) => self.move_focused(1),
            (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up) => self.move_focused(-1),
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => self.move_focused(half_page),
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => self.move_focused(-half_page),
            (KeyModifiers::NONE, KeyCode::PageDown) => self.move_focused(page),
            (KeyModifiers::NONE, KeyCode::PageUp) => self.move_focused(-page),
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
            (KeyModifiers::NONE, KeyCode::Enter) => self.open_selected_file(),
            (_, KeyCode::Char('q') | KeyCode::Esc) => Outcome::Dismiss,
            _ => Outcome::Consumed,
        }
    }

    /// Open the changed-files pane's selected file in the file diff view.
    ///
    /// Only from that pane: `Enter` on the graph is about a commit, not a
    /// file, and has no meaning here yet.
    ///
    /// The path asked for is [`crate::git::diff::FileStat::path`], which for a
    /// rename is the *destination*: that is the name the file has in this
    /// commit's tree, and the only one `file_diff` can find.
    fn open_selected_file(&mut self) -> Outcome {
        if self.focus != Pane::Files {
            return Outcome::Consumed;
        }
        let Some(commit) = self.selected_id() else {
            return Outcome::Consumed;
        };
        let DetailState::Ready(diff) = &self.detail else {
            return Outcome::Consumed;
        };
        let Some(path) = diff.files.get(self.file_selected).map(|f| f.path.clone()) else {
            return Outcome::Consumed;
        };

        self.mode = Mode::FileDiff;
        self.file_diff_scroll = 0;
        let want = (commit, path);
        // Reopening the file already loaded (or already asked for) must not
        // post a second request: there is no cancellation, so every duplicate
        // is a whole file diff the worker computes and this then throws away.
        // A previous *failure* is not reused — retrying is the only way back
        // from a transient one.
        let reuse = self.file_diff_for.as_ref() == Some(&want)
            && matches!(
                self.file_diff,
                FileDiffState::Ready(_) | FileDiffState::Loading(_)
            );
        if reuse {
            return Outcome::Redraw;
        }
        match self.worker.as_mut() {
            Some(w) if w.is_alive() => {
                w.request(crate::worker::Request::File {
                    commit: want.0,
                    path: want.1.clone(),
                });
                self.file_diff = FileDiffState::Loading(want.1.clone());
                self.file_diff_for = Some(want);
            }
            _ => {
                self.file_diff = FileDiffState::Failed(WORKER_GONE.to_string());
                self.file_diff_for = None;
            }
        }
        Outcome::Redraw
    }

    /// Handle one key while the file diff view is open.
    ///
    /// `Redraw` rather than `Consumed` wherever something moved: the host
    /// repaints a key by default, but the mode change and the scroll offset
    /// are both invisible to its `selected()` check, and this view is the
    /// whole overlay.
    fn file_diff_key(&mut self, key: KeyEvent) -> Outcome {
        let page = self.file_diff_rows.max(1);
        let half_page = (page / 2).max(1) as isize;
        let page = page as isize;
        match (key.modifiers, key.code) {
            // Unwind one layer. The overlay itself stays open; only `q`/`Esc`
            // in `Normal` closes it.
            (_, KeyCode::Char('q') | KeyCode::Esc) => {
                self.mode = Mode::Normal;
                Outcome::Redraw
            }
            (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down) => self.scroll_file_diff(1),
            (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up) => self.scroll_file_diff(-1),
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => self.scroll_file_diff(half_page),
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => self.scroll_file_diff(-half_page),
            (KeyModifiers::NONE, KeyCode::PageDown) => self.scroll_file_diff(page),
            (KeyModifiers::NONE, KeyCode::PageUp) => self.scroll_file_diff(-page),
            (KeyModifiers::NONE, KeyCode::Char('g') | KeyCode::Home) => {
                self.scroll_file_diff(isize::MIN)
            }
            (KeyModifiers::SHIFT, KeyCode::Char('G')) | (_, KeyCode::End) => {
                self.scroll_file_diff(isize::MAX)
            }
            _ => Outcome::Consumed,
        }
    }

    /// Scroll the file diff view by `delta` lines, clamped to the diff.
    fn scroll_file_diff(&mut self, delta: isize) -> Outcome {
        let total = match &self.file_diff {
            FileDiffState::Ready(h) => h.diff.lines.len(),
            _ => 0,
        };
        // The last screenful is the end: scrolling into blank space below the
        // diff would take as many keys to come back from as it took to reach.
        // Before the first frame there is no known height, and one row is the
        // safe assumption — it clamps to the diff either way.
        let visible = self.file_diff_rows.max(1);
        let last = total.saturating_sub(visible);
        let next = if delta < 0 {
            self.file_diff_scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.file_diff_scroll.saturating_add(delta as usize)
        }
        .min(last);
        if next == self.file_diff_scroll {
            return Outcome::Consumed;
        }
        self.file_diff_scroll = next;
        Outcome::Redraw
    }

    /// Handle one mouse event, routed by the pane it landed in.
    ///
    /// Coordinates are the terminal's own, and so are the rectangles in
    /// `layout`: the host renders this widget at an absolute `Rect` and hands
    /// the event over unchanged.
    pub fn on_mouse(&mut self, ev: MouseEvent) -> Outcome {
        // The file diff view covers the panes, so `layout` — which still holds
        // the last three-pane frame — must not route anything while it is up.
        if self.mode == Mode::FileDiff {
            return match ev.kind {
                MouseEventKind::ScrollDown => self.scroll_file_diff(WHEEL_ROWS),
                MouseEventKind::ScrollUp => self.scroll_file_diff(-WHEEL_ROWS),
                _ => Outcome::Consumed,
            };
        }
        if let Some(pane) = crate::ui::layout::pane_at(&self.layout, ev.column, ev.row) {
            if matches!(ev.kind, MouseEventKind::Down(_)) {
                if self.focus == pane {
                    return Outcome::Consumed;
                }
                self.focus = pane;
                return Outcome::Redraw;
            }
            // Scrolling acts on the pane under the pointer, not the focused
            // one — that is what a reader expects from a wheel. The focus
            // itself is left alone.
            return match ev.kind {
                MouseEventKind::ScrollDown => self.move_pane(pane, WHEEL_ROWS),
                MouseEventKind::ScrollUp => self.move_pane(pane, -WHEEL_ROWS),
                // Mouse capture reports motion continuously. Answering
                // `Consumed` without moving anything is what stops the host
                // repainting the overlay per report.
                _ => Outcome::Consumed,
            };
        }
        // No pane under the pointer: the overlay's own border, or the very
        // first frame not yet drawn. Fall back to the focused pane so the
        // wheel still does what `j`/`k` would.
        match ev.kind {
            MouseEventKind::ScrollDown => self.move_focused(WHEEL_ROWS),
            MouseEventKind::ScrollUp => self.move_focused(-WHEEL_ROWS),
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

        if self.mode == Mode::FileDiff {
            // The view is the whole overlay, and the pane layout below is not
            // drawn at all — `layout` keeps the last three-pane frame, which is
            // why `on_mouse` refuses to route by it in this mode.
            self.file_diff_rows = crate::ui::file_diff::draw_file_diff(
                buf,
                inner,
                &self.file_diff,
                self.file_diff_scroll,
            );
            return;
        }

        if self.row_count() == 0 {
            let msg = match self.error.as_deref() {
                Some(error) => format!("cannot read this repository: {error}"),
                None => "no commits yet".to_string(),
            };
            crate::ui::graph_view::draw_message(buf, inner, &msg);
            return;
        }
        let map = crate::ui::layout::split(inner);
        self.layout = map; // remembered for mouse routing
        self.viewport_rows = map.graph.height as usize;
        draw_rows(
            buf,
            map.graph,
            self.builder.nodes(),
            &self.decorations,
            self.first_row,
            self.selected,
        );
        // The row count comes back from the draw so the scroll can stop at
        // the end of the pane's content without this module having to keep a
        // second copy of how that content is laid out.
        self.detail_rows = crate::ui::commit_detail::draw_detail(
            buf,
            map.detail,
            self.selected_commit(),
            &self.detail,
            self.detail_scroll,
            self.focus == Pane::Detail,
        );
        crate::ui::file_list::draw_files(
            buf,
            map.files,
            &self.detail,
            self.file_selected,
            self.file_scroll,
            self.focus == Pane::Files,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::Fixture;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A successful commit reply, for feeding `accept_reply_for_test` without
    /// going through a real worker round trip.
    fn asd_git_reply_commit(id: gix::ObjectId) -> crate::worker::Reply {
        crate::worker::Reply::Commit {
            id,
            result: Ok(crate::git::diff::CommitDiff::default()),
        }
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
    fn poll_settles_to_false_once_the_initial_detail_has_arrived() {
        // Opening now posts a request for the newly selected commit, so the
        // very first poll may deliver it. Once that reply has landed and
        // nothing else is outstanding, poll must go back to reporting no
        // change.
        let (_fx, mut g) = graph_with(1, "state-poll");
        settle(&mut g);
        assert!(
            !g.poll(),
            "no background work is outstanding once the detail has settled"
        );
    }

    #[test]
    fn selecting_a_commit_asks_the_worker_and_poll_delivers_it() {
        let fx = Fixture::new("state-detail");
        std::fs::write(fx.path().join("a.txt"), "one\ntwo\n").unwrap();
        fx.git(&["add", "a.txt"]);
        fx.commit("first");

        let mut g = GitGraph::open(fx.path()).expect("fixture opens");
        // Opening selects the newest commit, which posts a request.
        assert!(matches!(g.detail(), DetailState::Loading));

        settle(&mut g);

        match g.detail() {
            DetailState::Ready(d) => {
                assert_eq!(d.files.len(), 1);
                assert_eq!(d.files[0].path, "a.txt");
            }
            other => panic!("expected a ready detail, got {other:?}"),
        }
    }

    #[test]
    fn poll_reports_false_when_nothing_arrived() {
        let fx = Fixture::new("state-poll-idle");
        std::fs::write(fx.path().join("a.txt"), "one\n").unwrap();
        fx.git(&["add", "a.txt"]);
        fx.commit("first");

        let mut g = GitGraph::open(fx.path()).unwrap();
        // Drain whatever the initial selection produced.
        settle(&mut g);
        assert!(
            !g.poll(),
            "a second poll with no outstanding work reports no change"
        );
    }

    #[test]
    fn a_reply_for_a_commit_no_longer_selected_is_discarded() {
        // Selection moves faster than the worker on a large repository; a late
        // reply must not overwrite the detail of the row the user is now on.
        let fx = Fixture::new("state-stale");
        for i in 0..3 {
            std::fs::write(fx.path().join("a.txt"), format!("{i}\n")).unwrap();
            fx.git(&["add", "a.txt"]);
            fx.commit(&format!("commit {i}"));
        }
        let mut g = GitGraph::open(fx.path()).unwrap();
        let newest = g.selected_id().expect("a commit is selected");

        g.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        let now_selected = g.selected_id().expect("still on a commit");
        assert_ne!(newest, now_selected);

        // Feed a reply for the OLD selection and confirm it is ignored.
        g.accept_reply_for_test(asd_git_reply_commit(newest));
        assert!(
            matches!(g.detail(), DetailState::Loading),
            "a stale reply must not become the current detail"
        );
    }

    #[test]
    fn a_reply_that_arrives_as_the_worker_dies_is_still_applied() {
        // There is no cancellation: an in-flight request always completes,
        // so its reply can still be sitting in the channel at the exact
        // moment the background thread notices its work channel closed and
        // exits. `poll` must apply that reply before it acts on the death,
        // not instead of it -- otherwise a perfectly good diff is thrown
        // away in favour of "unavailable". `close_requests_for_test`
        // reproduces this deterministically: it closes only the request
        // side of the channel, so the one request `open` already queued is
        // still delivered and answered before the thread sees the closed
        // channel and exits, leaving its reply sitting in `rx` unread.
        let fx = Fixture::new("state-die-with-reply");
        std::fs::write(fx.path().join("a.txt"), "one\ntwo\n").unwrap();
        fx.git(&["add", "a.txt"]);
        fx.commit("first");

        let mut g = GitGraph::open(fx.path()).expect("fixture opens");
        // `open` already posted a request for the newest (only) commit.
        assert!(matches!(g.detail(), DetailState::Loading));

        let worker = g.worker.as_mut().expect("worker started");
        let finished = worker.thread_finished_flag();
        worker.close_requests_for_test();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !finished.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "worker thread never exited"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // The thread is gone, but its final reply is still queued, unread.
        assert!(g.poll(), "applying the final reply must repaint");
        match g.detail() {
            DetailState::Ready(d) => {
                assert_eq!(d.files.len(), 1);
                assert_eq!(d.files[0].path, "a.txt");
            }
            other => {
                panic!("the reply that beat the death notice must still be applied, got {other:?}")
            }
        }

        // Nothing is left outstanding, and the worker's death was already
        // noticed: a second poll must settle rather than keep reporting
        // change forever.
        assert!(
            !g.poll(),
            "no outstanding work once the worker's death has been noticed"
        );
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

    /// Poll until the selected commit's detail has arrived.
    ///
    /// The worker is a real thread, so tests wait on its answer rather than
    /// sleeping a fixed amount and hoping.
    fn settle(g: &mut GitGraph) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while matches!(g.detail(), DetailState::Loading) {
            assert!(std::time::Instant::now() < deadline, "detail never arrived");
            g.poll();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Poll until the open file's diff has arrived.
    fn settle_file(g: &mut GitGraph) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while matches!(g.file_diff(), FileDiffState::Loading(_)) {
            assert!(
                std::time::Instant::now() < deadline,
                "file diff never arrived"
            );
            g.poll();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// A repository whose single commit touches `files` files, opened and
    /// polled until its detail has arrived.
    fn ready_graph(tag: &str, files: usize) -> (Fixture, GitGraph) {
        let fx = Fixture::new(tag);
        for i in 0..files {
            std::fs::write(fx.path().join(format!("f{i}.txt")), "one\n").unwrap();
            fx.git(&["add", "."]);
        }
        fx.commit("first");
        let mut g = GitGraph::open(fx.path()).unwrap();
        settle(&mut g);
        (fx, g)
    }

    #[test]
    fn tab_cycles_focus_through_the_three_panes() {
        let (_fx, mut g) = ready_graph("focus-cycle", 2);
        assert_eq!(g.focus(), Pane::Graph);
        g.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(g.focus(), Pane::Detail);
        g.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(g.focus(), Pane::Files);
        g.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(g.focus(), Pane::Graph, "Tab wraps");
    }

    #[test]
    fn back_tab_cycles_the_other_way() {
        let (_fx, mut g) = ready_graph("focus-backtab", 2);
        g.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(g.focus(), Pane::Files);
        g.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(g.focus(), Pane::Detail);
        g.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(g.focus(), Pane::Graph);
    }

    #[test]
    fn j_moves_the_file_selection_when_the_files_pane_has_focus() {
        let (_fx, mut g) = ready_graph("focus-files", 3);
        let commit_row = g.selected();
        g.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        g.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(g.focus(), Pane::Files);

        assert_eq!(g.file_selected(), 0);
        g.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(g.file_selected(), 1, "j moves within the file list");
        assert_eq!(
            g.selected(),
            commit_row,
            "and leaves the commit selection alone"
        );
    }

    #[test]
    fn the_file_selection_cannot_run_past_the_list() {
        let (_fx, mut g) = ready_graph("focus-clamp", 2);
        g.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        g.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        for _ in 0..20 {
            g.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        }
        assert_eq!(g.file_selected(), 1, "two files means the last index is 1");
    }

    #[test]
    fn a_click_moves_focus_to_the_pane_that_was_clicked() {
        let (_fx, mut g) = ready_graph("focus-click", 2);
        // Render once so the layout map is populated.
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        (&mut g).render(area, &mut buf);
        let map = g.layout_for_test();
        assert!(
            map.files.height > 0,
            "the fixture area is tall enough to split"
        );

        g.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: map.files.x + 1,
            row: map.files.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(g.focus(), Pane::Files);
    }

    #[test]
    fn switching_commits_resets_the_file_selection() {
        let fx = Fixture::new("focus-reset");
        std::fs::write(fx.path().join("a.txt"), "1\n").unwrap();
        fx.git(&["add", "."]);
        fx.commit("first");
        std::fs::write(fx.path().join("b.txt"), "2\n").unwrap();
        std::fs::write(fx.path().join("c.txt"), "3\n").unwrap();
        fx.git(&["add", "."]);
        fx.commit("second");

        let mut g = GitGraph::open(fx.path()).unwrap();
        settle(&mut g);
        g.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        g.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        g.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(g.file_selected(), 1);

        // Move to the older commit; its file list is different.
        g.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        // Focus is on Files, so move the commit selection via the graph pane.
        g.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(g.focus(), Pane::Graph);
        g.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        settle(&mut g);
        assert_eq!(
            g.file_selected(),
            0,
            "a new commit starts at its first file"
        );
    }

    #[test]
    fn the_wheel_scrolls_the_pane_under_the_pointer_and_leaves_focus_alone() {
        // Four files so the list has somewhere to move to, and a rendered
        // frame so the layout map can route by coordinate.
        let (_fx, mut g) = ready_graph("focus-wheel", 4);
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        (&mut g).render(area, &mut buf);
        let map = g.layout_for_test();

        let wheel = |kind, x, y| MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };

        // Focus stays on the graph throughout: the wheel is not a click.
        assert_eq!(g.focus(), Pane::Graph);
        g.on_mouse(wheel(
            MouseEventKind::ScrollDown,
            map.files.x + 1,
            map.files.y + 1,
        ));
        assert_eq!(g.file_selected(), 3, "the files pane scrolled");
        assert_eq!(g.focus(), Pane::Graph, "the wheel did not move focus");

        // And again where the movement is clamped away, which is the path
        // that returns early: the focus must still be where it was.
        g.on_mouse(wheel(
            MouseEventKind::ScrollDown,
            map.files.x + 1,
            map.files.y + 1,
        ));
        assert_eq!(g.file_selected(), 3);
        assert_eq!(g.focus(), Pane::Graph);

        // The wheel over the graph still moves the commit selection.
        g.on_mouse(wheel(
            MouseEventKind::ScrollDown,
            map.graph.x + 1,
            map.graph.y,
        ));
        assert_eq!(g.focus(), Pane::Graph);
        assert_eq!(
            g.file_selected(),
            3,
            "which is not the files pane's business"
        );
    }

    #[test]
    fn the_detail_pane_scrolls_but_not_past_its_own_content() {
        let (_fx, mut g) = ready_graph("focus-detail-scroll", 1);
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        (&mut g).render(area, &mut buf);

        g.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(g.focus(), Pane::Detail);
        // The detail pane here is 19 rows tall and its content is 6 rows, so
        // there is nothing to scroll to and the offset must stay put rather
        // than run off into blank space.
        for _ in 0..40 {
            g.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        }
        assert_eq!(
            g.detail_scroll, 0,
            "content shorter than the pane never scrolls"
        );
        assert_eq!(g.selected(), 0, "and the commit selection did not move");
    }

    #[test]
    fn a_repeat_key_still_moves_the_selection() {
        // The host forwards `Repeat` deliberately, so holding `j` keeps
        // scrolling. `on_key` must not filter it back out.
        let (_fx, mut g) = graph_with(3, "state-repeat");
        let mut k = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        k.kind = ratatui::crossterm::event::KeyEventKind::Repeat;
        g.on_key(k);
        assert_eq!(g.selected(), 1);
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

    /// A repository whose second commit changes `a.txt`, opened and settled,
    /// with focus already on the changed-files pane.
    fn graph_on_a_file(tag: &str, first: &str, second: &str) -> (Fixture, GitGraph) {
        let fx = Fixture::new(tag);
        std::fs::write(fx.path().join("a.txt"), first).unwrap();
        fx.git(&["add", "."]);
        fx.commit("first");
        std::fs::write(fx.path().join("a.txt"), second).unwrap();
        fx.git(&["add", "."]);
        fx.commit("second");

        let mut g = GitGraph::open(fx.path()).unwrap();
        settle(&mut g);
        g.on_key(key(KeyCode::Tab));
        g.on_key(key(KeyCode::Tab));
        assert_eq!(g.focus(), Pane::Files);
        (fx, g)
    }

    /// A file reply, for feeding `accept_reply_for_test` without a real
    /// worker round trip.
    fn asd_git_reply_file(commit: gix::ObjectId, path: &str) -> crate::worker::Reply {
        crate::worker::Reply::File {
            commit,
            path: path.to_string(),
            result: Ok(crate::worker::HighlightedDiff {
                diff: crate::git::diff::FileDiff {
                    path: path.to_string(),
                    lines: Vec::new(),
                    binary: false,
                    truncated: false,
                },
                spans: Vec::new(),
            }),
        }
    }

    #[test]
    fn enter_on_a_file_opens_its_diff_and_esc_returns() {
        let (_fx, mut g) = graph_on_a_file("mode-filediff", "1\n2\n3\n", "1\nTWO\n3\n");
        assert_eq!(g.mode(), Mode::Normal);

        assert!(matches!(g.on_key(key(KeyCode::Enter)), Outcome::Redraw));
        assert_eq!(g.mode(), Mode::FileDiff, "Enter opens the selected file");

        // Esc unwinds one layer, back to the list.
        assert!(matches!(g.on_key(key(KeyCode::Esc)), Outcome::Redraw));
        assert_eq!(g.mode(), Mode::Normal);

        // And `q` unwinds the same one layer rather than closing the overlay.
        g.on_key(key(KeyCode::Enter));
        assert_eq!(g.mode(), Mode::FileDiff);
        assert!(matches!(g.on_key(key(KeyCode::Char('q'))), Outcome::Redraw));
        assert_eq!(g.mode(), Mode::Normal);
    }

    #[test]
    fn esc_in_normal_mode_still_dismisses_the_overlay() {
        let (_fx, mut g) = ready_graph("mode-dismiss", 1);
        assert_eq!(g.mode(), Mode::Normal);
        assert!(matches!(g.on_key(key(KeyCode::Esc)), Outcome::Dismiss));
    }

    #[test]
    fn enter_outside_the_files_pane_opens_nothing() {
        let (_fx, mut g) = ready_graph("mode-enter-graph", 1);
        assert_eq!(g.focus(), Pane::Graph);
        assert!(matches!(g.on_key(key(KeyCode::Enter)), Outcome::Consumed));
        assert_eq!(g.mode(), Mode::Normal);
    }

    #[test]
    fn the_file_diff_arrives_through_poll() {
        let (_fx, mut g) = graph_on_a_file("mode-diffarrives", "1\n2\n3\n", "1\nTWO\n3\n");
        g.on_key(key(KeyCode::Enter));
        settle_file(&mut g);
        match g.file_diff() {
            FileDiffState::Ready(h) => {
                assert_eq!(h.diff.path, "a.txt");
                assert!(
                    h.diff
                        .lines
                        .iter()
                        .any(|l| matches!(l, crate::git::diff::DiffLine::Added { .. }))
                );
                assert_eq!(
                    h.spans.len(),
                    h.diff.lines.len(),
                    "the worker highlighted every line before the view saw it"
                );
            }
            other => panic!("expected a ready file diff, got {other:?}"),
        }
    }

    #[test]
    fn a_file_reply_for_another_request_is_discarded() {
        // No cancellation: a reply for a file the user has navigated away from
        // still arrives, and must not become the open file's diff.
        let (_fx, mut g) = graph_on_a_file("mode-stale-file", "1\n2\n3\n", "1\nTWO\n3\n");
        g.on_key(key(KeyCode::Enter));
        let commit = g.selected_id().expect("a commit is selected");

        assert!(
            !g.accept_reply_for_test(asd_git_reply_file(commit, "other.txt")),
            "another path is not this request"
        );
        assert!(matches!(g.file_diff(), FileDiffState::Loading(_)));

        let other_commit = gix::ObjectId::null(gix::hash::Kind::Sha1);
        assert!(
            !g.accept_reply_for_test(asd_git_reply_file(other_commit, "a.txt")),
            "the same path in another commit is not this request either"
        );
        assert!(matches!(g.file_diff(), FileDiffState::Loading(_)));

        assert!(g.accept_reply_for_test(asd_git_reply_file(commit, "a.txt")));
        assert!(matches!(g.file_diff(), FileDiffState::Ready(_)));
    }

    #[test]
    fn keys_in_the_file_diff_view_do_not_reach_the_panes_underneath() {
        // `j` here scrolls the diff. Letting it through would move the
        // changed-files selection out of sight, so closing the view would land
        // the reader on a file they never chose.
        let fx = Fixture::new("mode-capture");
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(fx.path().join(name), "1\n").unwrap();
        }
        fx.git(&["add", "."]);
        fx.commit("first");

        let mut g = GitGraph::open(fx.path()).unwrap();
        settle(&mut g);
        g.on_key(key(KeyCode::Tab));
        g.on_key(key(KeyCode::Tab));
        g.on_key(key(KeyCode::Enter));
        assert_eq!(g.mode(), Mode::FileDiff);

        let before = g.file_selected();
        let commit_row = g.selected();
        for _ in 0..5 {
            g.on_key(key(KeyCode::Char('j')));
        }
        assert_eq!(g.file_selected(), before, "the file list did not move");
        assert_eq!(g.selected(), commit_row, "nor did the commit selection");

        // And the same for the wheel, which routes by the last frame's pane
        // rectangles — none of which are on screen now.
        g.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(g.file_selected(), before);
    }

    #[test]
    fn the_file_diff_view_scrolls_within_its_own_diff() {
        let body: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        let (_fx, mut g) = graph_on_a_file("mode-scroll", "line 1\n", &body);
        g.on_key(key(KeyCode::Enter));
        settle_file(&mut g);
        let total = match g.file_diff() {
            FileDiffState::Ready(h) => h.diff.lines.len(),
            other => panic!("expected a ready file diff, got {other:?}"),
        };
        assert!(total > 20, "the fixture is longer than one screen: {total}");

        // A frame first: the step and the end both come from the last one.
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = Buffer::empty(area);
        (&mut g).render(area, &mut buf);
        let visible = g.file_diff_rows;
        assert!(visible > 0, "the view had room");

        assert!(matches!(g.on_key(key(KeyCode::Char('j'))), Outcome::Redraw));
        assert_eq!(g.file_diff_scroll, 1);
        g.on_key(key(KeyCode::Char('k')));
        assert_eq!(g.file_diff_scroll, 0);
        assert!(
            matches!(g.on_key(key(KeyCode::Char('k'))), Outcome::Consumed),
            "clamped away at the top"
        );

        g.on_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(
            g.file_diff_scroll,
            total - visible,
            "the last screenful is the end"
        );
        g.on_key(key(KeyCode::Char('g')));
        assert_eq!(g.file_diff_scroll, 0);
    }

    #[test]
    fn reopening_the_same_file_reuses_the_diff_already_fetched() {
        // There is no cancellation, so every duplicate request is a whole file
        // diff computed and then thrown away.
        let (_fx, mut g) = graph_on_a_file("mode-reopen", "1\n2\n3\n", "1\nTWO\n3\n");
        g.on_key(key(KeyCode::Enter));
        settle_file(&mut g);
        assert!(matches!(g.file_diff(), FileDiffState::Ready(_)));

        g.on_key(key(KeyCode::Esc));
        g.on_key(key(KeyCode::Enter));
        assert_eq!(g.mode(), Mode::FileDiff);
        assert!(
            matches!(g.file_diff(), FileDiffState::Ready(_)),
            "the diff already in hand is shown rather than fetched again"
        );
    }

    #[test]
    fn the_file_diff_view_replaces_the_three_panes() {
        let (_fx, mut g) = graph_on_a_file("mode-render", "1\nold\n3\n", "1\nnew\n3\n");
        g.on_key(key(KeyCode::Enter));
        settle_file(&mut g);

        let area = Rect::new(0, 0, 60, 20);
        let mut buf = Buffer::empty(area);
        (&mut g).render(area, &mut buf);
        let text = buffer_text(&buf, area);
        assert!(
            text.contains("a.txt"),
            "the file's name is the title: {text:?}"
        );
        assert!(
            text.contains("new"),
            "and its added line is drawn: {text:?}"
        );
        assert!(
            !text.contains("Changed Files"),
            "the panes underneath are not drawn: {text:?}"
        );

        // Back to Normal, the panes return.
        g.on_key(key(KeyCode::Esc));
        let mut buf = Buffer::empty(area);
        (&mut g).render(area, &mut buf);
        assert!(buffer_text(&buf, area).contains("Changed Files"));
    }

    #[test]
    fn rendering_the_file_diff_view_at_any_size_does_not_panic() {
        // `render` clamps its area and `draw_file_diff` clamps every write,
        // but the mode is a second path through the same thread that paints
        // every open session, so it gets the same sweep.
        let (_fx, mut g) = graph_on_a_file("mode-render-sweep", "1\n2\n3\n", "1\nTWO\n3\n");
        g.on_key(key(KeyCode::Enter));
        settle_file(&mut g);
        for &ox in &[0u16, 2] {
            for &oy in &[0u16, 1] {
                for width in 0..=12u16 {
                    for height in 0..=6u16 {
                        let area = Rect::new(ox, oy, width, height);
                        let mut buf = Buffer::empty(Rect::new(
                            0,
                            0,
                            ox.saturating_add(width),
                            oy.saturating_add(height),
                        ));
                        (&mut g).render(area, &mut buf);
                    }
                }
            }
        }
    }
}
