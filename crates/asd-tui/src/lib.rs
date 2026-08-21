//! `asd-tui`: terminal UI client (ratatui) — a session sidebar next to a live
//! terminal pane, switching between the local daemon's sessions (the layout in
//! `images/image.png`). Opened by `asd ui [session]`.
//!
//! Threading: the TUI thread owns the `!Send` [`GhosttyVt`] and ratatui; a
//! background thread ([`conn`]) owns the daemon connection and exchanges plain
//! data over channels — the same split as every other asd client.
//!
//! Keys: [`keymap::Keymap`] owns the declarative bindings used by both routing
//! and UI hints. The defaults forward everything except the `Ctrl+A` prefix
//! (screen-style): `j/k` or arrows switch sessions, `1-9` jump, `c`
//! creates, `r` renames (input modal), `x` kills (confirmation modal), `b`/`s`
//! hide the sidebar / bottom status bar (the latter frees the pane's bottom row
//! so an input line can reach the window edge, keeping the IME box from covering
//! it), `R` reconnects, `q` quits, `Ctrl+A` sends a literal Ctrl+A. The mouse
//! selects/kills in the sidebar and scrolls the pane (local scrollback, like
//! `asd attach`) — but when the focused session tracks the mouse (opencode,
//! vim, htop) the event is forwarded to it instead (SGR-encoded); Shift keeps
//! the mouse local, and Shift+PageUp/PageDown scroll too.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use asd_client::terminal::ProbeResult;
use asd_proto::{SessionInfo, TerminalAppearance};
use asd_vt::{GhosttyVt, KeyEvent, RenderSnapshot, VtBackend};
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent as CtKey, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::crossterm::execute;

mod conn;
mod key;
mod keymap;
mod modal;
mod platform;
mod ui;

use conn::{Cmd, Conn, ConnectionEvent, Ev};
use keymap::{KeyAction, KeyResolution, Keymap};
use modal::{Modal, RenameInput, validate_rename};

/// Scrollback kept by the local terminal.
const SCROLLBACK: usize = 10_000;
/// Wheel scroll step in lines.
const WHEEL_STEP: usize = 3;
/// Wheel scroll step in sidebar sessions.
const SIDEBAR_WHEEL_STEP: usize = 1;
/// Longest the pane defers a repaint while a program holds a synchronized-output
/// (`?2026`) update open, bounding a lost `?2026l` (matches typical terminals).
const SYNC_MAX: Duration = Duration::from_millis(150);
/// Minimum time between frames driven only by the continuous running shimmer.
/// Windows Terminal starts refreshing auto-detected URL locations after 100 ms
/// without output, but the scan and UI update are asynchronous. Leave a broad
/// idle window for its observed asynchronous refresh to complete.
const RUNNING_SHIMMER_FRAME_INTERVAL: Duration = Duration::from_millis(500);
/// Windows Terminal's trailing debounce before rebuilding auto-detected URL
/// coordinates. Once this much host-output quiet has elapsed, assume the
/// visible URL footprint may have been cached and invalidate it if it moves.
const HOST_URL_SCAN_DEBOUNCE: Duration = Duration::from_millis(100);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(30);
const FAST_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(5);
const FIRST_CONNECTION_GENERATION: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LoopTiming {
    shimmer_due: bool,
    poll_timeout: Duration,
}

type HostUrlMarker = (u16, u16, String);

#[derive(Default)]
struct HostLinkState {
    footprint: Vec<HostUrlMarker>,
    may_be_cached: bool,
}

impl HostLinkState {
    /// Observe the next visible pane frame. Returns true exactly when a URL
    /// footprint that the host may have scanned should be fully repainted.
    fn before_frame(&mut self, snapshot: Option<&RenderSnapshot>, host_quiet: Duration) -> bool {
        if host_quiet >= HOST_URL_SCAN_DEBOUNCE && !self.footprint.is_empty() {
            self.may_be_cached = true;
        }

        let footprint = snapshot.map(host_url_footprint).unwrap_or_default();
        let repaint = self.may_be_cached && self.footprint != footprint;
        self.footprint = footprint;
        if repaint {
            // The full repaint gives the host a fresh set of cells. Further
            // movement during continuous output needs no additional repaint.
            self.may_be_cached = false;
        }
        repaint
    }
}

fn host_url_footprint(snapshot: &RenderSnapshot) -> Vec<HostUrlMarker> {
    // Keep this list aligned with Windows Terminal's auto-link regex. Other
    // pattern kinds may be highlighted, but only this URI pattern is returned
    // by GetHyperlinkAtBufferPosition as a clickable destination.
    const SCHEMES: [&str; 4] = ["https://", "http://", "ftp://", "file://"];
    let mut urls = Vec::new();
    let cols = snapshot.cols as usize;
    let rows = snapshot.rows.min(snapshot.cells.len() as u16) as usize;
    let cell_count = cols.saturating_mul(rows);

    for index in 0..cell_count {
        let Some(scheme) = SCHEMES
            .iter()
            .find(|scheme| scheme_cells_match(snapshot, index, scheme.as_bytes()))
        else {
            continue;
        };
        let mut token = String::from(*scheme);
        for next in index + scheme.len()..cell_count {
            let Some(cell) = snapshot_cell(snapshot, next) else {
                break;
            };
            let text = cell.grapheme.as_str();
            if text.is_empty()
                || text
                    .chars()
                    .any(|character| character.is_whitespace() || "<>\"'".contains(character))
            {
                break;
            }
            token.push_str(text);
        }
        urls.push(((index / cols) as u16, (index % cols) as u16, token));
    }
    urls
}

fn snapshot_cell(snapshot: &RenderSnapshot, index: usize) -> Option<&asd_vt::CellSnapshot> {
    let cols = snapshot.cols as usize;
    (cols != 0)
        .then_some(())
        .and_then(|()| snapshot.cells.get(index / cols))
        .and_then(|row| row.get(index % cols))
}

fn scheme_cells_match(snapshot: &RenderSnapshot, start: usize, scheme: &[u8]) -> bool {
    scheme.iter().enumerate().all(|(offset, expected)| {
        snapshot_cell(snapshot, start + offset).is_some_and(|cell| {
            let grapheme = cell.grapheme.as_bytes();
            grapheme.len() == 1 && grapheme[0] == *expected
        })
    })
}

fn loop_timing(
    last_frame_flush: Instant,
    now: Instant,
    has_running_fx: bool,
    fast_path: bool,
    next_running_expiry: Option<Duration>,
) -> LoopTiming {
    let base_poll_timeout = if fast_path {
        FAST_EVENT_POLL_INTERVAL
    } else {
        EVENT_POLL_INTERVAL
    };
    LoopTiming {
        shimmer_due: has_running_fx
            && now.saturating_duration_since(last_frame_flush) >= RUNNING_SHIMMER_FRAME_INTERVAL,
        poll_timeout: next_running_expiry
            .map_or(base_poll_timeout, |expiry| base_poll_timeout.min(expiry)),
    }
}

fn wall_clock_tick_due(displayed_ms: u64, current_ms: u64, visible: bool) -> bool {
    visible && displayed_ms / 1_000 != current_ms / 1_000
}

fn aged_idle_ms(session: &SessionInfo, elapsed_since_list: Duration) -> u64 {
    let elapsed_ms = u64::try_from(elapsed_since_list.as_millis()).unwrap_or(u64::MAX);
    session.idle_ms.saturating_add(elapsed_ms)
}

fn session_running_after(session: &SessionInfo, elapsed_since_list: Duration) -> bool {
    session.running && aged_idle_ms(session, elapsed_since_list) < asd_proto::IDLE_SETTLE_MS
}

fn running_time_left(session: &SessionInfo, elapsed_since_list: Duration) -> Option<Duration> {
    session_running_after(session, elapsed_since_list).then(|| {
        Duration::from_millis(asd_proto::IDLE_SETTLE_MS - aged_idle_ms(session, elapsed_since_list))
    })
}

#[derive(Clone, Default)]
struct RunningActivity {
    deadlines: HashMap<String, RunningDeadline>,
}

#[derive(Clone, Copy)]
struct RunningDeadline {
    at: Instant,
    from_local_output: bool,
}

impl RunningActivity {
    fn with_list(&self, sessions: &[SessionInfo], observed_at: Instant) -> Self {
        let deadlines = sessions
            .iter()
            .filter_map(|session| {
                let listed =
                    running_time_left(session, Duration::ZERO).map(|left| RunningDeadline {
                        at: observed_at + left,
                        from_local_output: false,
                    });
                let local = self
                    .deadlines
                    .get(&session.name)
                    .copied()
                    .filter(|deadline| deadline.from_local_output && observed_at < deadline.at);
                let deadline = match (listed, local) {
                    (Some(listed), Some(local)) if local.at > listed.at => local,
                    (Some(listed), _) => listed,
                    (None, Some(local)) => local,
                    (None, None) => return None,
                };
                Some((session.name.clone(), deadline))
            })
            .collect();
        Self { deadlines }
    }

    fn with_output(&self, name: &str, observed_at: Instant) -> Self {
        let deadline = RunningDeadline {
            at: observed_at + Duration::from_millis(asd_proto::IDLE_SETTLE_MS),
            from_local_output: true,
        };
        let deadlines = self
            .deadlines
            .iter()
            .filter(|(session, _)| session.as_str() != name)
            .map(|(session, deadline)| (session.clone(), *deadline))
            .chain(std::iter::once((name.to_string(), deadline)))
            .collect();
        Self { deadlines }
    }

    fn with_rename(&self, old: &str, new: &str) -> Self {
        let deadlines = self
            .deadlines
            .iter()
            .map(|(session, deadline)| {
                let session = if session == old {
                    new.to_string()
                } else {
                    session.clone()
                };
                (session, *deadline)
            })
            .collect();
        Self { deadlines }
    }

    fn is_running(&self, name: &str, now: Instant) -> bool {
        self.deadlines
            .get(name)
            .is_some_and(|deadline| now < deadline.at)
    }

    fn expired_names(&self, now: Instant) -> Vec<String> {
        self.deadlines
            .iter()
            .filter(|(name, _)| !self.is_running(name, now))
            .map(|(name, _)| name.clone())
            .collect()
    }

    fn without_expired(&self, now: Instant) -> Self {
        let deadlines = self
            .deadlines
            .iter()
            .filter(|(_, deadline)| now < deadline.at)
            .map(|(name, deadline)| (name.clone(), *deadline))
            .collect();
        Self { deadlines }
    }

    fn next_expiry(&self, now: Instant, excluded_name: Option<&str>) -> Option<Duration> {
        self.deadlines
            .iter()
            .filter(|(name, _)| excluded_name != Some(name.as_str()))
            .map(|(_, deadline)| deadline.at.saturating_duration_since(now))
            .min()
    }
}

fn sessions_with_activity(
    sessions: &[SessionInfo],
    activity: &RunningActivity,
    now: Instant,
) -> Vec<SessionInfo> {
    sessions
        .iter()
        .cloned()
        .map(|session| SessionInfo {
            running: activity.is_running(&session.name, now),
            ..session
        })
        .collect()
}

fn sessions_with_running(
    sessions: &[SessionInfo],
    names: &[String],
    running: bool,
) -> Vec<SessionInfo> {
    sessions
        .iter()
        .cloned()
        .map(|session| {
            if names.contains(&session.name) {
                SessionInfo { running, ..session }
            } else {
                session
            }
        })
        .collect()
}

/// Find a rename of the active live session between two list samples. Names
/// are mutable; the process id plus creation timestamp identify this daemon
/// session across that change without confusing it with a newly created name.
fn renamed_active_session(
    active: Option<&str>,
    previous: &[SessionInfo],
    current: &[SessionInfo],
) -> Option<(String, String)> {
    let old = previous
        .iter()
        .find(|session| Some(session.name.as_str()) == active)?;
    current
        .iter()
        .find(|session| {
            session.pid == old.pid
                && session.created_ms == old.created_ms
                && session.name != old.name
        })
        .map(|session| (old.name.clone(), session.name.clone()))
}

/// A drag selection anchored in **absolute screen-space rows** (0 = oldest
/// scrollback line, same coordinate system as `scrollback_rows`) so the
/// highlight tracks the text while scrolling — the CLI attach client's model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sel {
    anchor: (u16, usize),
    head: (u16, usize),
}

impl Sel {
    /// Project into viewport coordinates, clipped to the visible rows;
    /// `None` when entirely off-screen.
    fn viewport(
        self,
        scrollback: usize,
        scroll: usize,
        cols: u16,
        rows: u16,
    ) -> Option<ui::Selection> {
        // Order the ends row-major in screen space.
        let (a, b) = if (self.anchor.1, self.anchor.0) <= (self.head.1, self.head.0) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        let base = scrollback as isize - scroll as isize;
        let ay = a.1 as isize - base;
        let by = b.1 as isize - base;
        let rows = rows as isize;
        if rows <= 0 || by < 0 || ay >= rows {
            return None;
        }
        let start = if ay < 0 { (0, 0) } else { (a.0, ay as u16) };
        let end = if by >= rows {
            (cols.saturating_sub(1), (rows - 1) as u16)
        } else {
            (b.0, by as u16)
        };
        Some(ui::Selection { start, end })
    }
}

/// Screen-space row of viewport row `y` while scrolled `scroll` lines up over
/// a `scrollback`-deep history.
fn screen_row(scrollback: usize, scroll: usize, y: u16) -> usize {
    scrollback.saturating_sub(scroll) + usize::from(y)
}

/// The top screen row for sidebar session index `i` given the scroll `offset`,
/// or `None` if that session is scrolled out of the `side` viewport (each row
/// is two lines tall).
fn row_y(side: ratatui::layout::Rect, i: usize, offset: usize) -> Option<u16> {
    let pos = i.checked_sub(offset)?;
    let y = side.top() + (pos as u16) * 2;
    (y + 1 < side.bottom()).then_some(y)
}

pub(crate) struct App {
    socket: PathBuf,
    conn: Conn,
    ev_rx: Receiver<ConnectionEvent>,
    ev_tx: Sender<ConnectionEvent>,
    connection_generation: u64,

    pub sessions: Vec<SessionInfo>,
    /// Monotonic idle deadlines derived from the last list response and from
    /// live output for the attached session. This removes list-poll lag without
    /// letting an older list sample override output observed locally afterward.
    running_activity: RunningActivity,
    /// URL coordinates in the pane that Windows Terminal may have auto-detected.
    /// When a scanned footprint moves, the next frame fully repaints the current
    /// host buffer. Auto-detected click targets may lag until the host rescans.
    host_links: HostLinkState,
    /// The attached session's name.
    pub active: Option<String>,
    /// The active sidebar selection whose exclusive TUI view was taken by
    /// another `asd ui`. It stays selected so choosing it again is an explicit
    /// takeover, while the pane renders a placard instead of stale terminal
    /// cells.
    pub view_revoked: Option<String>,
    /// Local terminal for the attached session (recreated per attach).
    vt: Option<GhosttyVt>,
    /// Local scrollback offset: 0 = follow live output.
    pub scroll: usize,
    /// Terminal grid offered by the pane.
    grid: (u16, u16),
    /// The size the local terminal is currently at. Usually [`grid`], but the
    /// pty belongs to whichever client resized it last, so it can be smaller.
    vt_grid: (u16, u16),
    /// Whole-terminal size (cols, rows), for recomputing the layout on a
    /// sidebar resize/toggle without a `Resize` event.
    term_size: (u16, u16),
    /// Current sidebar width (draggable; [`ui::MIN_SIDEBAR`]..[`ui::MAX_SIDEBAR`]).
    sidebar_w: u16,
    /// Sessions scrolled past the top of the sidebar.
    sidebar_scroll: usize,
    /// Sidebar hidden (Ctrl+A b) — the pane takes the full width.
    sidebar_hidden: bool,
    /// Bottom status bar hidden (Ctrl+A s) — the pane takes the full height, so
    /// a session's input line can reach the window's true bottom. This lets the
    /// OS input-method candidate box float off the bottom edge instead of
    /// covering the bottom row (the asd status bar otherwise costs a row).
    status_hidden: bool,
    /// True while dragging the sidebar↔pane divider with the mouse.
    dragging_divider: bool,
    /// Drag selection over the pane, if any.
    sel: Option<Sel>,
    /// True between mouse press and release while dragging a selection.
    selecting: bool,
    /// The last text copied from a pane selection, for right-click paste — asd
    /// grabs the mouse, so the host terminal's own right-click paste never
    /// reaches us. This is what was selected *here*, not the system clipboard.
    clipboard: Option<String>,
    /// The host cursor's end-of-frame state `(x, y, visible)` in host cells,
    /// recomputed by `ui::draw` every frame and emitted by `FrameBuf::finish`
    /// as the frame's closing bytes. Visible for the pane's shell cursor and
    /// the rename caret — the IME popup and codex/vim anchor to the REAL
    /// cursor (a painted cell broke both). Positioned-but-hidden when the
    /// focused session hides its own cursor (pi / Claude Code draw their own
    /// caret), so the OS IME box still floats at the app's input. `None` (no
    /// session, scrolled back, kill-confirm modal) keeps the cursor hidden.
    cursor_tail: Option<(u16, u16, bool)>,

    pub daemon_up: bool,
    pub notice: Option<String>,
    /// An open modal overlay (rename input or kill confirmation); captures all
    /// keys until it closes.
    pub modal: Option<Modal>,
    /// Declarative global/PREFIX bindings and their pending leader state.
    pub keymap: Keymap,
    pub now_ms: u64,
    /// The daemon host's latest resource reading, for the bottom bar. `None`
    /// before the first reply arrives.
    pub metrics: Option<asd_proto::HostSample>,

    /// Session named on the command line, consumed by the first auto-select.
    preferred: Option<String>,
    /// Defaults reported by the real terminal hosting this TUI. Reused for
    /// every session switch; the daemon adopts each first known channel.
    terminal_appearance: TerminalAppearance,
    /// Keys typed during the short startup color probe, forwarded after the
    /// first session is selected rather than swallowed.
    startup_input: Vec<u8>,
    /// The session this UI itself runs inside ($ASD_SESSION, set by the
    /// daemon at spawn): attaching it would be a render feedback loop, so it
    /// is never selectable here.
    pub self_session: Option<String>,
    /// The previous session's last frame, shown while a switch converges so
    /// the pane never flashes black (double buffering across attaches).
    cache: Option<RenderSnapshot>,
    /// Terminals of recently viewed sessions, parked on switch-away (small
    /// LRU). Switching back shows the parked terminal's last frame instantly
    /// — the boo-style feel — while the fresh attach converges behind it.
    parked: Vec<(String, GhosttyVt)>,
    /// Keep showing `cache` while a switch is in flight. The attach Snapshot
    /// is a complete, exact replay (single frame), so the reveal is
    /// deterministic — the moment the dump is fed (boo's `.screen` marker,
    /// no settle heuristics). The deadline only bounds a switch whose
    /// Snapshot never arrives.
    pane_hold: Option<std::time::Instant>,
    /// The last fully-rendered pane frame. Reused for redraws that don't change
    /// the terminal (e.g. sidebar shimmer ticks) and while the program is mid
    /// atomic-update (`synchronized_output`), so the pane is regenerated only on
    /// real output/scroll/switch and never painted half-drawn.
    pane_cache: Option<RenderSnapshot>,
    /// The pane's terminal content changed since `pane_cache` was built.
    pane_needs_render: bool,
    /// When the current synchronized-output (`?2026`) window started, bounding
    /// how long the pane defers a repaint if a lost `?2026l` never clears it.
    sync_since: Option<std::time::Instant>,
    /// Sidebar row effects (tachyonfx), keyed by session name: sweep-in on
    /// newly listed sessions, a brief accent fade on selection.
    row_fx: Vec<(String, tachyonfx::Effect)>,
    /// A continuous color shimmer for each *running* session's row text, keyed
    /// by session name. Daemon snapshots turn it on; local idle aging turns it
    /// off exactly at the shared settle threshold instead of waiting for the
    /// next list poll. The UI's own host session is excluded (it always
    /// produces output — the TUI itself — so it would always shimmer).
    running_fx: Vec<(String, tachyonfx::Effect)>,
    /// Previous frame instant, for effect timing.
    last_frame: std::time::Instant,
    dirty: bool,
    quit: bool,
}

/// Shared per-frame byte buffer backing the ratatui terminal, so one frame
/// reaches the terminal as ONE `write`.
///
/// A stock stdout backend flushes mid-frame: crossterm's `execute!` flushes on
/// every cursor command and `Stdout`'s LineWriter splits the cell diff into
/// ~1 KiB chunks, so each frame left the process as 4–7 separate writes with
/// the visible cursor parked on whatever cell was painted last. Any terminal
/// that renders at such a boundary — guaranteed possible over SSH, certain
/// without DEC-2026 support — briefly showed the cursor inside the sidebar
/// shimmer or on the echoed keystroke: the historical flicker. Composing the
/// whole frame here — `?2026h ?25l <cells> <CUP><?25h|?25l> ?2026l`, boo's
/// frame shape — and writing it once is atomic on 2026 terminals; on anything
/// else the cursor is hidden during the body, so a torn frame can at worst
/// blank it for one refresh, never misplace it.
#[derive(Clone, Default)]
struct FrameBuf(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

impl FrameBuf {
    /// Open a frame: synchronized-update begin, cursor hidden for the body.
    fn begin(&self) {
        let mut b = self.0.borrow_mut();
        b.clear();
        b.extend_from_slice(b"\x1b[?2026h\x1b[?25l");
    }

    /// Preserve the current host screen buffer before repainting moved links.
    /// This deliberately emits no bytes: leaving and re-entering alternate
    /// screen visibly flashes on remote xterm-style hosts. The caller still
    /// forces a same-size full repaint.
    fn preserve_host_screen(&self) {
        let _ = self;
    }

    /// Close a frame with the cursor tail and hand it to the terminal as a
    /// single write.
    fn finish(&self, tail: Option<(u16, u16, bool)>) -> std::io::Result<()> {
        use std::io::Write;
        let mut b = self.0.borrow_mut();
        b.extend_from_slice(&cursor_tail(tail));
        b.extend_from_slice(b"\x1b[?2026l");
        let mut out = std::io::stdout();
        out.write_all(&b)?;
        out.flush()
    }
}

impl std::io::Write for FrameBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    // ratatui/crossterm flush after every cursor command; the frame goes out
    // only in `FrameBuf::finish`.
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// End-of-frame cursor bytes: place, then set visibility — CUP before
/// `?25h`/`?25l` (boo's order) so even a frame torn right here never shows the
/// cursor mid-body. `None` keeps it hidden.
fn cursor_tail(tail: Option<(u16, u16, bool)>) -> Vec<u8> {
    match tail {
        Some((x, y, visible)) => format!(
            "\x1b[{};{}H\x1b[?25{}",
            u32::from(y) + 1,
            u32::from(x) + 1,
            if visible { 'h' } else { 'l' }
        )
        .into_bytes(),
        None => b"\x1b[?25l".to_vec(),
    }
}

/// Input bytes for pasted `text`, wrapped in the bracketed-paste markers when
/// the session program is in mode 2004.
///
/// The host terminal brackets a paste for us, but crossterm strips the markers
/// off before handing over `Event::Paste` — so forwarding the text alone drops
/// the one signal that says "this was pasted", and every line break in it
/// lands as Enter. A shell runs each line, an agent prompt submits at the
/// blank line. Putting the markers back is what makes a multi-line paste stay
/// one piece of text.
///
/// Only when the program asked for them: a program that does not know mode
/// 2004 shows `[200~` as text instead.
///
/// An end marker inside the text is dropped rather than passed on, as xterm
/// does — otherwise pasted content could close the bracket early and have its
/// tail read as keystrokes.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    asd_client::terminal::paste_bytes(text.as_bytes(), bracketed)
}

/// Open the TUI against `socket`; `session` preselects one by name. The
/// daemon must already be running (the `asd ui` wrapper ensures it).
pub fn run(socket: PathBuf, session: Option<String>) -> anyhow::Result<()> {
    // Restore the terminal even on a kill / hangup (external `kill`, closed tab,
    // dropped SSH) — a panic hook only fires for Rust panics, not signals. Must
    // run before raw mode so it captures the cooked termios.
    platform::install_terminating_signal_restore();
    platform::spawn_tty_watchdog();
    // Manual `ratatui::init`, with the backend writing into a `FrameBuf`
    // instead of stdout so each frame is flushed as a single write.
    ratatui::crossterm::terminal::enable_raw_mode()?;
    let probe = match asd_client::terminal::probe_terminal_colors() {
        Ok(probe) => probe,
        Err(error) => {
            let _ = ratatui::crossterm::terminal::disable_raw_mode();
            return Err(error.into());
        }
    };
    let _ = execute!(
        std::io::stdout(),
        ratatui::crossterm::terminal::EnterAlternateScreen
    );
    let frame = FrameBuf::default();
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(frame.clone()))?;
    // Manual construction skips `ratatui::init`'s panic hook, so chain our own:
    // mouse capture + bracketed paste off first (a panic must not leave the
    // terminal spewing `ESC[<..M` reports on every mouse move), then ratatui's
    // screen restore (leave the alt screen, disable raw mode).
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(
            std::io::stdout(),
            event::DisableBracketedPaste,
            event::DisableMouseCapture
        );
        ratatui::restore();
        prev_hook(info);
    }));
    let _ = execute!(
        std::io::stdout(),
        event::EnableMouseCapture,
        event::EnableBracketedPaste
    );

    let result = event_loop(&mut terminal, &frame, socket, session, probe);

    let _ = execute!(
        std::io::stdout(),
        event::DisableBracketedPaste,
        event::DisableMouseCapture
    );
    ratatui::restore();
    // The last frame may have left the cursor hidden (viewing a pi/Claude-style
    // session that hides its own): `?25` is global terminal state that survives
    // leaving the alt screen, so re-show it for the shell.
    let _ = execute!(std::io::stdout(), ratatui::crossterm::cursor::Show);
    result
}

fn event_loop(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<FrameBuf>>,
    frame: &FrameBuf,
    socket: PathBuf,
    preferred: Option<String>,
    probe: ProbeResult,
) -> anyhow::Result<()> {
    let (ev_tx, ev_rx) = channel::<ConnectionEvent>();
    let connection_generation = FIRST_CONNECTION_GENERATION;
    let conn = Conn::spawn(socket.clone(), connection_generation, ev_tx.clone());
    let size = terminal.size()?;
    let sidebar_w = ui::SIDEBAR_W;
    let grid = ui::pane_grid(
        ratatui::layout::Rect::new(0, 0, size.width, size.height),
        sidebar_w,
        false,
        false,
    );

    let mut app = App {
        socket,
        conn,
        ev_rx,
        ev_tx,
        connection_generation,
        sessions: Vec::new(),
        running_activity: RunningActivity::default(),
        host_links: HostLinkState::default(),
        active: None,
        view_revoked: None,
        vt: None,
        scroll: 0,
        grid,
        vt_grid: grid,
        term_size: (size.width, size.height),
        sidebar_w,
        sidebar_scroll: 0,
        sidebar_hidden: false,
        status_hidden: false,
        dragging_divider: false,
        sel: None,
        selecting: false,
        clipboard: None,
        cursor_tail: None,
        daemon_up: false,
        notice: None,
        modal: None,
        keymap: Keymap::default(),
        now_ms: now_ms(),
        metrics: None,
        preferred,
        terminal_appearance: probe.appearance,
        startup_input: probe.input,
        self_session: std::env::var("ASD_SESSION").ok(),
        cache: None,
        parked: Vec::new(),
        pane_hold: None,
        pane_cache: None,
        pane_needs_render: true,
        sync_since: None,
        row_fx: Vec::new(),
        running_fx: Vec::new(),
        last_frame: std::time::Instant::now(),
        dirty: true,
        quit: false,
    };
    let mut last_frame_flush = Instant::now();

    while !app.quit {
        // The terminal's own size is the authority, not the resize *event*.
        // ratatui re-reads it before every draw, so the sidebar/pane/status
        // layout always follows the real window; the pane grid we negotiate with
        // the daemon came from a value updated only on `Event::Resize`, and a
        // SIGWINCH that lands before crossterm installs its handler is lost.
        // The two would then disagree for the rest of the session — a correct
        // frame around a session left at the startup size. One ioctl per
        // iteration (~33/s at the poll rate below) keeps them in step, and makes
        // the `Event::Resize` arm below nothing but a wake-up.
        if let Ok(size) = terminal.size()
            && (size.width, size.height) != app.term_size
        {
            app.term_size = (size.width, size.height);
            app.apply_layout();
        }
        while let Ok(event) = app.ev_rx.try_recv() {
            if let Some(ev) = event_for_generation(app.connection_generation, event) {
                app.on_conn_event(ev);
            }
        }
        let now = Instant::now();
        let wall_now_ms = now_ms();
        app.dirty |= wall_clock_tick_due(app.now_ms, wall_now_ms, !app.status_hidden);
        app.dirty |= app.expire_running_sessions(now);
        let timing = loop_timing(
            last_frame_flush,
            now,
            !app.running_fx.is_empty(),
            app.pane_hold.is_some() || !app.row_fx.is_empty(),
            app.next_running_expiry(now),
        );
        app.dirty |= timing.shimmer_due;
        if app.dirty {
            let repaint_host_links = app.host_link_repaint_needed(
                Instant::now().saturating_duration_since(last_frame_flush),
            );
            app.now_ms = wall_now_ms;
            // One frame = one write: `FrameBuf` wraps the cell diff in
            // `?2026h ?25l` … `<CUP><?25h|?25l> ?2026l` and flushes it in a
            // single `write_all`, so a shimmer redraw can neither
            // drag the cursor across the sidebar cells nor toggle its
            // visibility at a flush boundary — both historical flicker sources
            // (separate `execute!` flushes made every frame 4–7 writes, and
            // terminals without DEC-2026 render freely between writes). The
            // tail is `app.cursor_tail`, recomputed by `ui::draw`.
            frame.begin();
            if repaint_host_links {
                frame.preserve_host_screen();
                terminal.resize(ratatui::layout::Rect::new(
                    0,
                    0,
                    app.term_size.0,
                    app.term_size.1,
                ))?;
            }
            terminal.draw(|f| ui::draw(f, &mut app))?;
            frame.finish(app.cursor_tail)?;
            last_frame_flush = Instant::now();
            // Transient effects and a pane hold need the 5 ms fast path. The
            // continuous running shimmer is scheduled separately so it leaves
            // a host-output idle window without slowing input polling.
            app.dirty = !app.row_fx.is_empty() || app.pane_hold.is_some();
        }
        // Tighten the loop while a switch converges or effects animate:
        // conn events are only drained between polls, so a long poll adds
        // whole quanta of latency to the dump/repaint pipeline.
        if event::poll(timing.poll_timeout)? {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => app.on_key(k),
                Event::Mouse(m) => app.on_mouse(m, terminal.size()?),
                Event::Paste(text) => {
                    // A modal owns input: route a paste into the rename field,
                    // swallow it under the kill-confirm — never leak it to the
                    // session.
                    if let Some(Modal::Rename(input)) = app.modal.as_mut() {
                        for c in text.chars() {
                            input.insert(c);
                        }
                        app.dirty = true;
                    } else if app.modal.is_none() {
                        if app.scroll != 0 {
                            app.pane_needs_render = true;
                        }
                        app.scroll = 0;
                        let bytes = app.paste(&text);
                        app.send(Cmd::Input(bytes));
                    }
                }
                // Only a wake-up: the loop head above re-reads the real size and
                // applies it, on this pass and on every pass after — including
                // the resizes whose event never reaches us.
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
    app.send(Cmd::Shutdown);
    Ok(())
}

impl App {
    fn host_link_repaint_needed(&mut self, host_quiet: Duration) -> bool {
        let snapshot = self.snapshot();
        self.host_links.before_frame(snapshot.as_ref(), host_quiet)
    }

    fn expire_running_sessions(&mut self, now: Instant) -> bool {
        let expired = self.running_activity.expired_names(now);
        if expired.is_empty() {
            return false;
        }

        self.sessions = sessions_with_running(&self.sessions, &expired, false);
        self.running_activity = self.running_activity.without_expired(now);
        true
    }

    fn next_running_expiry(&self, now: Instant) -> Option<Duration> {
        self.running_activity
            .next_expiry(now, self.self_session.as_deref())
    }

    /// Schedule (or replace) a sidebar effect for a session row.
    fn add_fx(&mut self, name: String, fx: tachyonfx::Effect) {
        self.row_fx.retain(|(n, _)| n != &name);
        self.row_fx.push((name, fx));
    }

    fn sidebar_capacity(&self) -> usize {
        let total = ratatui::layout::Rect::new(0, 0, self.term_size.0, self.term_size.1);
        let (side, _, _) = ui::areas(
            total,
            self.sidebar_w,
            self.sidebar_hidden,
            self.status_hidden,
        );
        (side.height / 2) as usize
    }

    /// Sessions scrolled off the top of the sidebar. Drawing, effects, and
    /// mouse hit-testing all use this, so a click maps to the row the user sees.
    pub(crate) fn sidebar_offset(&self) -> usize {
        ui::scroll_sidebar_offset(
            self.sidebar_scroll,
            0,
            self.sessions.len(),
            self.sidebar_capacity(),
        )
    }

    fn clamp_sidebar_scroll(&mut self) {
        self.sidebar_scroll = self.sidebar_offset();
    }

    fn ensure_active_sidebar_visible(&mut self) {
        let Some(active_idx) = self
            .active
            .as_deref()
            .and_then(|a| self.sessions.iter().position(|s| s.name == a))
        else {
            self.clamp_sidebar_scroll();
            return;
        };
        let next = ui::sidebar_offset_for_selection(
            self.sidebar_scroll,
            active_idx,
            self.sessions.len(),
            self.sidebar_capacity(),
        );
        if next != self.sidebar_scroll {
            self.sidebar_scroll = next;
            self.dirty = true;
        }
    }

    fn scroll_sidebar_by(&mut self, delta: isize) {
        let next = ui::scroll_sidebar_offset(
            self.sidebar_scroll,
            delta,
            self.sessions.len(),
            self.sidebar_capacity(),
        );
        if next != self.sidebar_scroll {
            self.sidebar_scroll = next;
            self.dirty = true;
        }
    }

    /// Advance and paint the sidebar effects; called once per drawn frame.
    /// Two layers: the transient row effects (sweep-in / selection fade) and
    /// the continuous breathing border on every running session's row.
    pub(crate) fn process_fx(
        &mut self,
        buf: &mut ratatui::buffer::Buffer,
        side: ratatui::layout::Rect,
    ) {
        let now = std::time::Instant::now();
        let delta: tachyonfx::Duration = now.duration_since(self.last_frame).into();
        self.last_frame = now;

        self.process_running_fx(buf, side, delta);

        if self.row_fx.is_empty() {
            return;
        }
        let offset = self.sidebar_offset();
        let sessions = &self.sessions;
        self.row_fx.retain_mut(|(name, fx)| {
            let Some(i) = sessions.iter().position(|s| &s.name == name) else {
                return false; // session gone
            };
            // Scrolled out of view: advance the effect into an empty rect so it
            // still expires (and gets dropped), but paint nothing off-screen.
            let rect = row_y(side, i, offset)
                .map(|y| {
                    ratatui::layout::Rect::new(side.left(), y, side.width.saturating_sub(1), 2)
                })
                .unwrap_or_else(|| ratatui::layout::Rect::new(side.left(), side.top(), 0, 0));
            fx.process(delta, buf, rect);
            !fx.done()
        });
    }

    /// Keep the local terminal the same size as the session's pty.
    ///
    /// The pty belongs to whichever client resized it last, so another client
    /// with a smaller window shrinks it under us. Our terminal would stay at
    /// our own pane size, and the columns and rows the session no longer writes
    /// would keep whatever was last drawn there — nothing revisits them, so the
    /// leftovers sit on screen until something forces a full repaint. Following
    /// the real size makes the snapshot honest about how much of the pane the
    /// session actually covers; the pane renderer blanks the rest.
    fn follow_session_size(&mut self) {
        let Some(active) = self.active.clone() else {
            return;
        };
        let Some(info) = self.sessions.iter().find(|s| s.name == active) else {
            return;
        };
        let want = (info.cols.max(1), info.rows.max(1));
        if want == self.vt_grid {
            return;
        }
        if let Some(vt) = &mut self.vt {
            vt.resize(want.0, want.1);
            self.vt_grid = want;
            // The cached frame describes the old geometry.
            self.pane_cache = None;
            self.dirty = true;
        }
    }

    /// Keep one color-shimmer effect per running (non-self) session, then
    /// advance each over its two sidebar rows. The text is drawn (in accent)
    /// by the sidebar renderer; the effect only rotates its hue, via a
    /// `CellFilter::Text` limited to written cells. The right-edge rule column
    /// is excluded from the processed area so the separator stays put.
    fn process_running_fx(
        &mut self,
        buf: &mut ratatui::buffer::Buffer,
        side: ratatui::layout::Rect,
        delta: tachyonfx::Duration,
    ) {
        let self_name = self.self_session.clone();
        let running: Vec<String> = self
            .sessions
            .iter()
            .filter(|s| s.running && self_name.as_deref() != Some(s.name.as_str()))
            .map(|s| s.name.clone())
            .collect();
        // Drop effects for sessions that stopped running (or vanished); add one
        // for each newly running session.
        self.running_fx.retain(|(n, _)| running.contains(n));
        for name in &running {
            if !self.running_fx.iter().any(|(n, _)| n == name) {
                self.running_fx.push((name.clone(), running_shimmer()));
            }
        }
        let offset = self.sidebar_offset();
        let sessions = &self.sessions;
        for (name, fx) in self.running_fx.iter_mut() {
            let Some(i) = sessions.iter().position(|s| &s.name == name) else {
                continue;
            };
            let Some(y) = row_y(side, i, offset) else {
                continue; // scrolled out of view
            };
            // Shimmer only the name/title text: skip the marker + ordinal on
            // the left (up to ROW_TEXT_X) and the right rule, so neither the
            // ordinal, the selection frame, nor the separator is hue-shifted.
            let rect = ratatui::layout::Rect::new(
                side.left() + ui::ROW_TEXT_X,
                y,
                side.width.saturating_sub(ui::ROW_TEXT_X + 1),
                2,
            );
            fx.process(delta, buf, rect);
        }
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.conn.cmd_tx.send(cmd);
    }

    /// Pasted `text` as input bytes for the attached session, bracketed when
    /// that session wants it (see [`paste_bytes`]). Our own VT tracks the
    /// session's modes, so it is the one that knows.
    fn paste(&mut self, text: &str) -> Vec<u8> {
        let bracketed = self.vt.as_mut().is_some_and(|vt| vt.bracketed_paste());
        paste_bytes(text, bracketed)
    }

    /// Current frame of the attached terminal, if any. Re-clamps the scroll
    /// offset first: the scrollback can shrink under it (e.g. the session
    /// entered the alternate screen), and a stale offset would leave the
    /// scroll indicator lying about a view that is actually live.
    pub fn snapshot(&mut self) -> Option<RenderSnapshot> {
        // Across a switch, keep the previous frame up until the new attach's
        // Snapshot has been fed (or the safety deadline expires).
        if let Some(deadline) = self.pane_hold {
            if std::time::Instant::now() < deadline {
                if self.cache.is_some() {
                    return self.cache.clone();
                }
            } else {
                self.pane_hold = None;
                self.cache = None;
            }
        }
        let vt = self.vt.as_mut()?;
        self.scroll = self.scroll.min(vt.scrollback_rows());

        // Defer a repaint while the program is mid atomic-update (synchronized
        // output, `?2026`): keep showing the last complete frame so the pane is
        // never painted half-drawn. A deadline bounds a stuck/lost `?2026l`.
        let in_sync = if vt.synchronized_output() {
            let now = std::time::Instant::now();
            let since = *self.sync_since.get_or_insert(now);
            now.duration_since(since) < SYNC_MAX
        } else {
            self.sync_since = None;
            false
        };

        // Reuse the cache when the terminal hasn't changed (e.g. a sidebar
        // shimmer tick redrew the frame) or while an atomic update is in flight;
        // only regenerate on a real output/scroll/switch change.
        if (!self.pane_needs_render || in_sync) && self.pane_cache.is_some() {
            return self.pane_cache.clone();
        }

        let scroll = self.scroll;
        vt.set_scroll(scroll);
        let snap = vt.render_snapshot();
        self.pane_needs_render = false;
        self.pane_cache = Some(snap.clone());
        Some(snap)
    }

    /// The drag selection projected into pane-viewport coordinates.
    pub fn sel_viewport(&mut self) -> Option<ui::Selection> {
        let sel = self.sel?;
        let (cols, rows) = self.grid;
        let scroll = self.scroll;
        let scrollback = self.vt.as_mut().map(|vt| vt.scrollback_rows())?;
        sel.viewport(scrollback, scroll, cols, rows)
    }

    fn select(&mut self, name: String) {
        if self.active.as_deref() == Some(&name) && self.view_revoked.as_deref() != Some(&name) {
            self.ensure_active_sidebar_visible();
            return;
        }
        // tmux's $TMUX idea: never attach the session hosting this UI — the
        // render feedback loop floods the pty (and everyone watching).
        if self.self_session.as_deref() == Some(&name) {
            self.notice = Some(format!("{name} hosts this UI — not attachable"));
            self.dirty = true;
            return;
        }
        // What's on screen right now, as the fallback hold frame.
        let old_frame = self.vt.as_mut().map(|vt| {
            vt.set_scroll(0);
            vt.render_snapshot()
        });
        // Park the terminal we're leaving (its last frame is the instant
        // preview when the user switches back).
        if let (Some(old_name), Some(old_vt)) = (self.active.take(), self.vt.take()) {
            self.parked.retain(|(n, _)| n != &old_name);
            self.parked.push((old_name, old_vt));
            const PARKED_MAX: usize = 4;
            if self.parked.len() > PARKED_MAX {
                self.parked.remove(0);
            }
        }
        self.active = Some(name.clone());
        self.view_revoked = None;
        self.ensure_active_sidebar_visible();
        // Hold a frame on screen while the new attach converges — never draw
        // the empty terminal (a black flash). Prefer the target session's own
        // parked frame (instant, boo-style); fall back to what was showing.
        self.cache = self
            .parked
            .iter_mut()
            .find(|(n, _)| n == &name)
            .map(|(_, vt)| {
                vt.set_scroll(0);
                vt.render_snapshot()
            })
            .or(old_frame);
        // Safety bound only — the real reveal is the Snapshot arriving. A
        // heavy session's dump can take a while to generate and feed, so the
        // bound is generous; a failed attach clears the hold via its own
        // event well before this.
        self.pane_hold = Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
        self.vt = Some(GhosttyVt::new(self.grid.0, self.grid.1, SCROLLBACK));
        self.vt_grid = self.grid;
        self.scroll = 0;
        self.sel = None;
        self.selecting = false;
        self.notice = None;
        // The new terminal has no cached frame yet; force a fresh render.
        self.pane_cache = None;
        self.pane_needs_render = true;
        self.sync_since = None;
        self.add_fx(
            name.clone(),
            tachyonfx::fx::fade_from_fg(
                ratatui::style::Color::Rgb(0xF3, 0xB2, 0x4C),
                (250, tachyonfx::Interpolation::SineOut),
            ),
        );
        self.send(Cmd::Attach {
            name,
            cols: self.grid.0,
            rows: self.grid.1,
            appearance: self.terminal_appearance,
        });
        self.dirty = true;
    }

    fn select_by_offset(&mut self, delta: isize) {
        if self.sessions.is_empty() {
            return;
        }
        let cur = self
            .active
            .as_deref()
            .and_then(|a| self.sessions.iter().position(|s| s.name == a))
            .unwrap_or(0) as isize;
        let n = self.sessions.len() as isize;
        // Step over the session hosting this UI (see `select`).
        let mut next = cur;
        for _ in 0..self.sessions.len() {
            next = (next + delta).rem_euclid(n);
            let candidate = &self.sessions[next as usize].name;
            if self.self_session.as_deref() != Some(candidate) {
                return self.select(candidate.clone());
            }
        }
    }

    fn on_conn_event(&mut self, ev: Ev) {
        match ev {
            Ev::Up => {
                self.daemon_up = true;
                self.notice = None;
            }
            Ev::Down(reason) => {
                self.daemon_up = false;
                self.notice = Some(reason);
                self.active = None;
                self.view_revoked = None;
                self.vt = None;
                self.pane_cache = None;
                // Otherwise the bar keeps showing the last CPU/memory/network
                // reading, frozen, beside a clock that is local and keeps
                // ticking -- a stale number that looks live.
                self.metrics = None;
            }
            Ev::Sessions(list) => {
                // A list reply can overtake the session thread's ViewRenamed
                // notification because they share a socket queue but originate
                // from different daemon threads. Match the stable live-session
                // identity before replacing the old list so that a rename is
                // never mistaken for "old vanished, auto-attach the new one".
                let renamed_active =
                    renamed_active_session(self.active.as_deref(), &self.sessions, &list);
                if let Some((old_name, new_name)) = renamed_active {
                    self.apply_rename(&old_name, &new_name);
                }
                let observed_at = Instant::now();
                self.running_activity = self.running_activity.with_list(&list, observed_at);
                let list = sessions_with_activity(&list, &self.running_activity, observed_at);
                // Drop parked terminals of sessions that no longer exist.
                self.parked
                    .retain(|(n, _)| list.iter().any(|s| &s.name == n));
                // Newly listed sessions sweep into the sidebar.
                for s in &list {
                    if !self.sessions.iter().any(|old| old.name == s.name) {
                        self.add_fx(
                            s.name.clone(),
                            tachyonfx::fx::sweep_in(
                                tachyonfx::Motion::LeftToRight,
                                10,
                                0,
                                ratatui::style::Color::Rgb(0x0B, 0x0D, 0x11),
                                (350, tachyonfx::Interpolation::QuadOut),
                            ),
                        );
                    }
                }
                self.sessions = list;
                self.clamp_sidebar_scroll();
                self.follow_session_size();
                // The attached session vanished (killed elsewhere): fall back
                // to the first remaining one.
                if let Some(a) = &self.active
                    && !self.sessions.iter().any(|s| &s.name == a)
                {
                    self.active = None;
                    self.view_revoked = None;
                    self.vt = None;
                }
                if self.active.is_none() {
                    let not_self = |name: &str| self.self_session.as_deref() != Some(name);
                    let pick = self
                        .preferred
                        .take_if(|p| self.sessions.iter().any(|s| &s.name == p))
                        .filter(|p| not_self(p))
                        .or_else(|| {
                            self.sessions
                                .iter()
                                .find(|s| not_self(&s.name))
                                .map(|s| s.name.clone())
                        });
                    if let Some(name) = pick {
                        self.select(name);
                    }
                }
            }
            Ev::Metrics(sample) => {
                self.metrics = sample;
                // Only dirty the frame when the bar is visible. Comparing
                // `self.metrics != sample` instead would not work: every
                // reply carries a freshly recomputed `sampled_age_ms`, so
                // equality never holds and this event would always dirty
                // the frame regardless. Gating on visibility matches
                // `wall_clock_tick_due`'s own `visible` gate and keeps a
                // hidden bar as quiet under this event as it already is
                // under `Ev::Sessions`, which arrives on the same tick.
                if !self.status_hidden {
                    self.dirty = true;
                }
            }
            Ev::Created(name) => self.select(name),
            Ev::Bytes {
                name,
                data,
                snapshot,
            } => {
                // Bytes from a session we already left can still be in flight.
                if self.active.as_deref() != Some(&name) {
                    return;
                }
                if !snapshot {
                    self.running_activity =
                        self.running_activity.with_output(&name, Instant::now());
                    self.sessions =
                        sessions_with_running(&self.sessions, std::slice::from_ref(&name), true);
                }
                if snapshot {
                    self.view_revoked = None;
                    // A snapshot is a full redraw into a clean terminal.
                    self.vt = Some(GhosttyVt::new(self.grid.0, self.grid.1, SCROLLBACK));
                    self.vt_grid = self.grid;
                    self.scroll = 0;
                    self.sel = None;
                    // The old cache belongs to a different terminal — drop it.
                    self.pane_cache = None;
                    self.sync_since = None;
                }
                if let Some(vt) = &mut self.vt {
                    vt.feed(&data);
                    // The daemon owns the session VT and is the only query
                    // responder. A local mirror must drain and discard its
                    // effects or multiple viewers would answer one PTY query.
                    let _ = vt.take_pty_responses();
                }
                // Probe input is replayed only after the first Snapshot, when
                // the session's bracketed-paste mode is known. This prevents a
                // multiline paste during startup from becoming raw Enter keys.
                if snapshot && !self.startup_input.is_empty() {
                    let input = std::mem::take(&mut self.startup_input);
                    let bracketed = self.vt.as_mut().is_some_and(|vt| vt.bracketed_paste());
                    let input = asd_client::terminal::prepare_probe_input(input, bracketed);
                    self.send(Cmd::Input(input));
                }
                // The terminal changed: the pane must regenerate next draw.
                self.pane_needs_render = true;
                if snapshot {
                    // The dump is an exact replay of the daemon's terminal
                    // (asd-vt's two-pass snapshot), generated at this pane's
                    // size — feeding it IS convergence. Reveal immediately,
                    // boo's `.screen`-marker semantics.
                    self.pane_hold = None;
                    self.cache = None;
                }
            }
            Ev::SessionEnded { name, msg } => {
                if self.active.as_deref() == Some(&name) {
                    self.notice = Some(format!("{name} — {msg}"));
                    // Whatever the pane was holding for is not coming.
                    self.pane_hold = None;
                    self.cache = None;
                }
            }
            Ev::ViewRevoked {
                previous_name,
                name,
            } => {
                if self.active.as_deref() == Some(&previous_name)
                    || self.active.as_deref() == Some(&name)
                {
                    self.apply_rename(&previous_name, &name);
                    self.view_revoked = Some(name.clone());
                    self.notice = Some(format!("{name} — view opened in another asd ui"));
                    self.vt = None;
                    self.cache = None;
                    self.pane_hold = None;
                    self.pane_cache = None;
                    self.pane_needs_render = true;
                    self.sync_since = None;
                    self.scroll = 0;
                    self.sel = None;
                    self.selecting = false;
                    self.parked
                        .retain(|(parked, _)| parked != &previous_name && parked != &name);
                }
            }
            Ev::ViewRenamed { old_name, new_name } => {
                self.apply_rename(&old_name, &new_name);
            }
            Ev::Renamed(res) => {
                // ViewRenamed applies a success; Ack only confirms completion.
                // Surface a validation race or other rejection here.
                if let Err(msg) = res {
                    self.notice = Some(format!("rename failed: {msg}"));
                }
            }
        }
        self.dirty = true;
    }

    fn on_key(&mut self, k: CtKey) {
        self.dirty = true;
        // An open modal captures every key until it closes.
        if self.modal.is_some() {
            self.on_modal_key(k);
            return;
        }
        match self.keymap.resolve(&k) {
            KeyResolution::PassThrough => {
                if let Some(ev) = key::map_key(&k) {
                    self.forward(ev);
                }
            }
            KeyResolution::Consumed => {}
            KeyResolution::Action(action) => self.apply_key_action(action),
        }
    }

    fn apply_key_action(&mut self, action: KeyAction) {
        match action {
            KeyAction::SelectNext => self.select_by_offset(1),
            KeyAction::SelectPrevious => self.select_by_offset(-1),
            KeyAction::JumpTo(ordinal) => {
                let index = usize::from(ordinal.saturating_sub(1));
                if let Some(name) = self.sessions.get(index).map(|session| session.name.clone()) {
                    self.select(name);
                }
            }
            KeyAction::Create => self.send(Cmd::Create),
            KeyAction::ToggleSidebar => {
                self.sidebar_hidden = !self.sidebar_hidden;
                self.apply_layout();
            }
            KeyAction::ToggleStatus => {
                self.status_hidden = !self.status_hidden;
                self.apply_layout();
            }
            KeyAction::Kill => {
                if let Some(name) = self.active.clone() {
                    self.modal = Some(Modal::KillConfirm { target: name });
                }
            }
            KeyAction::Rename => {
                if let Some(name) = self.active.clone() {
                    self.modal = Some(Modal::Rename(RenameInput::new(name)));
                }
            }
            KeyAction::Reconnect => self.reconnect(),
            KeyAction::Quit => self.quit = true,
            KeyAction::ScrollPageUp => self.scroll_by(self.grid.1.saturating_sub(1) as isize),
            KeyAction::ScrollPageDown => self.scroll_by(-(self.grid.1.saturating_sub(1) as isize)),
            KeyAction::SendLeaderLiteral => {
                if let Some(event) = key::map_key(&self.keymap.literal_leader()) {
                    self.forward(event);
                }
            }
            KeyAction::CancelPrefix => {}
        }
    }

    /// Route a key to the open modal. Editing keys mutate the input in place;
    /// terminal decisions (submit / cancel / kill) are taken after the borrow
    /// ends so `self` is free to act on.
    fn on_modal_key(&mut self, k: CtKey) {
        enum Act {
            Keep,
            Close,
            Kill(String),
            TryRename(String, String),
        }
        let act = match self.modal.as_mut() {
            Some(Modal::KillConfirm { target }) => match k.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    Act::Kill(target.clone())
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => Act::Close,
                _ => Act::Keep,
            },
            Some(Modal::Rename(input)) => match k.code {
                KeyCode::Esc => Act::Close,
                KeyCode::Enter => Act::TryRename(input.target.clone(), input.text()),
                KeyCode::Char(c) => {
                    input.insert(c);
                    Act::Keep
                }
                KeyCode::Backspace => {
                    input.backspace();
                    Act::Keep
                }
                KeyCode::Delete => {
                    input.delete();
                    Act::Keep
                }
                KeyCode::Left => {
                    input.left();
                    Act::Keep
                }
                KeyCode::Right => {
                    input.right();
                    Act::Keep
                }
                KeyCode::Home => {
                    input.home();
                    Act::Keep
                }
                KeyCode::End => {
                    input.end();
                    Act::Keep
                }
                _ => Act::Keep,
            },
            None => Act::Keep,
        };
        match act {
            Act::Keep => {}
            Act::Close => self.modal = None,
            Act::Kill(name) => {
                self.modal = None;
                self.send(Cmd::Kill { name });
            }
            Act::TryRename(target, new) => {
                let existing: Vec<String> = self.sessions.iter().map(|s| s.name.clone()).collect();
                match validate_rename(&new, &existing, &target) {
                    Ok(()) => {
                        self.modal = None;
                        self.submit_rename(target, new);
                    }
                    Err(e) => {
                        if let Some(Modal::Rename(input)) = self.modal.as_mut() {
                            input.error = Some(e);
                        }
                    }
                }
            }
        }
    }

    /// Send a rename request. The daemon's `ViewRenamed` event applies the name
    /// only after registry validation succeeds, so a concurrent duplicate-name
    /// rejection cannot leave the pane tagged with a name it never acquired.
    fn submit_rename(&mut self, target: String, new: String) {
        if target == new {
            return;
        }
        self.send(Cmd::Rename {
            name: target,
            new_name: new,
        });
    }

    /// Apply a rename observed locally or announced by the daemon. Keeping this
    /// separate from `submit_rename` prevents an external rename from being
    /// echoed back as a second protocol command.
    fn apply_rename(&mut self, target: &str, new: &str) {
        if target == new {
            return;
        }
        if self.active.as_deref() == Some(target) {
            self.active = Some(new.to_string());
        }
        if self.view_revoked.as_deref() == Some(target) {
            self.view_revoked = Some(new.to_string());
        }
        for s in &mut self.sessions {
            if s.name == target {
                s.name = new.to_string();
            }
        }
        self.running_activity = self.running_activity.with_rename(target, new);
        // The daemon lists sessions by name. Mirror that ordering immediately
        // so the next refresh cannot move the active row out of the viewport.
        self.sessions.sort_by(|a, b| a.name.cmp(&b.name));
        self.ensure_active_sidebar_visible();
        self.dirty = true;
    }

    fn forward(&mut self, ev: KeyEvent) {
        // Typing snaps back to the live bottom and clears any selection.
        if self.scroll != 0 {
            self.pane_needs_render = true;
        }
        self.scroll = 0;
        self.sel = None;
        if let Some(vt) = &mut self.vt {
            let bytes = vt.encode_key(ev);
            if !bytes.is_empty() {
                self.send(Cmd::Input(bytes));
            }
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.vt.as_mut().map(|vt| vt.scrollback_rows()).unwrap_or(0);
        let next = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
        if next != self.scroll {
            self.scroll = next;
            self.pane_needs_render = true;
            self.dirty = true;
        }
    }

    fn on_mouse(&mut self, m: MouseEvent, size: ratatui::layout::Size) {
        // A modal owns all input while open: swallow mouse events (there are no
        // modal-relevant mouse actions) so a click can't select/kill/scroll or
        // start a selection behind the overlay.
        if self.modal.is_some() {
            return;
        }
        let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
        let (_, pane, _) = ui::areas(
            area,
            self.sidebar_w,
            self.sidebar_hidden,
            self.status_hidden,
        );
        let wheel_target = ui::wheel_target(
            area,
            self.sidebar_w,
            self.sidebar_hidden,
            self.status_hidden,
            m.column,
            m.row,
        );
        let in_pane = m.column >= pane.left()
            && m.column < pane.right()
            && m.row >= pane.top()
            && m.row < pane.bottom();
        let on_divider = ui::divider_col(self.sidebar_w, self.sidebar_hidden) == Some(m.column);
        // When the focused session is tracking the mouse (opencode, vim, htop),
        // forward the event to it instead of using the wheel/click for local
        // scroll/selection — otherwise the app never receives mouse input (e.g.
        // opencode can't wheel-scroll). Only in the live view (not scrolled back
        // into history) and over the pane, not while Shift is held (Shift stays
        // local: host-native selection / scrollback) and not mid local gesture.
        // Encodes SGR (1006), which such apps enable; a session without SGR falls
        // through to local handling.
        if self.scroll == 0
            && in_pane
            && !m.modifiers.contains(KeyModifiers::SHIFT)
            && !self.dragging_divider
            && !self.selecting
        {
            let modes = self.vt.as_mut().and_then(|vt| {
                if vt.is_mouse_tracking() {
                    Some(vt.mouse_modes())
                } else {
                    None
                }
            });
            if let Some(modes) = modes
                && modes.iter().any(|&x| x == 1006 || x == 1015 || x == 1016)
            {
                if let Some(report) = encode_sgr_mouse(
                    m.kind,
                    m.modifiers,
                    m.column - pane.left(),
                    m.row - pane.top(),
                    &modes,
                ) {
                    self.send(Cmd::Input(report));
                }
                // The session owns the mouse in the live view: don't also
                // scroll/select locally, even for an event it didn't want.
                return;
            }
        }
        match m.kind {
            MouseEventKind::ScrollUp => match wheel_target {
                ui::WheelTarget::Sidebar => self.scroll_sidebar_by(-(SIDEBAR_WHEEL_STEP as isize)),
                ui::WheelTarget::Pane => self.scroll_by(WHEEL_STEP as isize),
                ui::WheelTarget::None => {}
            },
            MouseEventKind::ScrollDown => match wheel_target {
                ui::WheelTarget::Sidebar => self.scroll_sidebar_by(SIDEBAR_WHEEL_STEP as isize),
                ui::WheelTarget::Pane => self.scroll_by(-(WHEEL_STEP as isize)),
                ui::WheelTarget::None => {}
            },
            // Grabbing the divider begins a live sidebar resize (consumed by the
            // TUI — never a selection). Left button only: right/middle clicks
            // must not select, kill, or start a drag-selection.
            MouseEventKind::Down(MouseButton::Left) if on_divider => {
                self.dragging_divider = true;
                self.dirty = true;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((i, kill)) = ui::sidebar_hit(
                    area,
                    self.sidebar_w,
                    self.sidebar_hidden,
                    self.sessions.len(),
                    self.sidebar_offset(),
                    m.column,
                    m.row,
                ) {
                    let name = self.sessions[i].name.clone();
                    if kill {
                        if self.self_session.as_deref() == Some(&name) {
                            // Never kill the session hosting this UI (same guard
                            // as `select`) — it would tear the UI down.
                            self.notice =
                                Some(format!("{name} hosts this UI — can't kill it here"));
                        } else {
                            // Same path as Ctrl+A x: confirm first, never kill
                            // outright.
                            self.modal = Some(Modal::KillConfirm { target: name });
                        }
                    } else {
                        self.select(name);
                    }
                    self.dirty = true;
                } else if in_pane && self.vt.is_some() {
                    // Start a drag selection anchored in screen space (the
                    // attach client's model): it tracks the text, not the
                    // viewport, while scrolling.
                    let sb = self.vt.as_mut().map(|vt| vt.scrollback_rows()).unwrap_or(0);
                    let cell = (
                        m.column - pane.left(),
                        screen_row(sb, self.scroll, m.row - pane.top()),
                    );
                    self.sel = Some(Sel {
                        anchor: cell,
                        head: cell,
                    });
                    self.selecting = true;
                    self.dirty = true;
                }
            }
            // Live sidebar resize: the divider follows the mouse, clamped.
            MouseEventKind::Drag(_) if self.dragging_divider => {
                let w = ui::sidebar_from_drag(m.column, self.term_size.0);
                if w != self.sidebar_w {
                    self.sidebar_w = w;
                    self.apply_layout();
                }
            }
            MouseEventKind::Up(_) if self.dragging_divider => {
                self.dragging_divider = false;
                self.dirty = true;
            }
            MouseEventKind::Drag(_) if self.selecting => {
                if let Some(sel) = &mut self.sel {
                    let sb = self.vt.as_mut().map(|vt| vt.scrollback_rows()).unwrap_or(0);
                    let x = m
                        .column
                        .saturating_sub(pane.left())
                        .min(pane.width.saturating_sub(1));
                    let y = m
                        .row
                        .saturating_sub(pane.top())
                        .min(pane.height.saturating_sub(1));
                    sel.head = (x, screen_row(sb, self.scroll, y));
                    self.dirty = true;
                }
            }
            MouseEventKind::Up(_) if self.selecting => {
                self.selecting = false;
                // Releasing copies the selection (OSC 52 through the host
                // terminal) and clears the highlight; a plain click leaves
                // nothing behind. Screen-space coords are scroll-independent, so
                // the copy captures the whole range even off-view. Keep the text
                // for right-click paste too.
                let text = self
                    .sel
                    .take()
                    .filter(|s| s.anchor != s.head)
                    .and_then(|sel| {
                        let vt = self.vt.as_mut()?;
                        let text = vt.selection_text_screen(
                            (sel.anchor.0, sel.anchor.1 as u32),
                            (sel.head.0, sel.head.1 as u32),
                        );
                        (!text.is_empty()).then_some(text)
                    });
                if let Some(text) = text {
                    use std::io::Write;
                    let mut out = std::io::stdout();
                    let _ = out.write_all(&asd_vt::clip::osc52_copy(&text));
                    let _ = out.flush();
                    self.clipboard = Some(text);
                }
                self.dirty = true;
            }
            // Right-click pastes what was last copied here into the session — asd
            // grabs the mouse, so the host terminal's own right-click paste can't
            // reach us. Goes in as a paste, same as one from the host. (A
            // mouse-tracking session gets the right-click forwarded above; this
            // arm is reached for a plain shell prompt.)
            MouseEventKind::Down(MouseButton::Right) if in_pane => {
                if let Some(text) = self.clipboard.clone() {
                    let bytes = self.paste(&text);
                    self.send(Cmd::Input(bytes));
                }
            }
            _ => {}
        }
    }

    /// Recompute the pane grid from the current terminal size + sidebar state;
    /// if it changed, resize the local VT and tell the daemon. Called after a
    /// sidebar drag, an `Ctrl+A b` toggle, or a terminal resize. Reuses the
    /// tear-free pane path (`pane_needs_render`) so no half-frame shows.
    fn apply_layout(&mut self) {
        let total = ratatui::layout::Rect::new(0, 0, self.term_size.0, self.term_size.1);
        let grid = ui::pane_grid(
            total,
            self.sidebar_w,
            self.sidebar_hidden,
            self.status_hidden,
        );
        if grid != self.grid {
            self.grid = grid;
            if let Some(vt) = &mut self.vt {
                vt.resize(grid.0, grid.1);
                self.vt_grid = grid;
            }
            self.send(Cmd::Resize {
                cols: grid.0,
                rows: grid.1,
            });
        }
        self.clamp_sidebar_scroll();
        self.pane_needs_render = true;
        self.dirty = true;
    }

    /// Tear down the old connection actor and start a fresh one.
    fn reconnect(&mut self) {
        self.send(Cmd::Shutdown);
        self.connection_generation = self
            .connection_generation
            .checked_add(1)
            .expect("connection generation overflow");
        self.conn = Conn::spawn(
            self.socket.clone(),
            self.connection_generation,
            self.ev_tx.clone(),
        );
        self.notice = None;
        self.active = None;
        self.vt = None;
        // A stale reading from the old daemon must not survive the reconnect
        // and be shown as if it were current.
        self.metrics = None;
        self.dirty = true;
    }
}

fn event_for_generation(current: u64, event: ConnectionEvent) -> Option<Ev> {
    (event.generation == current).then_some(event.event)
}

/// A looping color shimmer for a running session's row: the text's hue rotates
/// a full turn, forever, so the (saturated-accent) row text cycles through the
/// rainbow. A left-to-right sweep pattern staggers the hue across columns, so
/// the color travels along the text rather than changing everywhere at once.
/// Foreground only — the background is never touched — and restricted to text
/// cells (`CellFilter::Text`) so blank cells stay put. Linear timing keeps the
/// rotation at a constant, non-strobing speed.
fn running_shimmer() -> tachyonfx::Effect {
    use tachyonfx::pattern::SweepPattern;
    use tachyonfx::{CellFilter, Interpolation, fx};
    fx::repeating(
        fx::hsl_shift_fg([360.0, 0.0, 0.0], (2000, Interpolation::Linear))
            .with_pattern(SweepPattern::left_to_right(160)),
    )
    .with_filter(CellFilter::Text)
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Encode a crossterm mouse event as an SGR (mode 1006) mouse report, to forward
/// to a session that has mouse tracking on. `col`/`row` are 0-based
/// pane-relative; the report uses 1-based coordinates. `modes` are the session's
/// enabled DEC mouse modes: motion (drag/move) is only reported when the session
/// asked for it (1002 button-event / 1003 any-event), so a plain click-tracking
/// app isn't spammed. Returns `None` for events the session's modes don't want.
fn encode_sgr_mouse(
    kind: MouseEventKind,
    mods: KeyModifiers,
    col: u16,
    row: u16,
    modes: &[u16],
) -> Option<Vec<u8>> {
    let button = |b: MouseButton| match b {
        MouseButton::Left => 0u16,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    // (SGR button code, is-release)
    let (mut cb, release) = match kind {
        MouseEventKind::Down(b) => (button(b), false),
        MouseEventKind::Up(b) => (button(b), true),
        MouseEventKind::Drag(b) => {
            if !modes.iter().any(|&m| m == 1002 || m == 1003) {
                return None;
            }
            (button(b) + 32, false) // + motion bit
        }
        MouseEventKind::Moved => {
            if !modes.contains(&1003) {
                return None;
            }
            (3 + 32, false) // no button + motion
        }
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    // Modifier bits (Shift is handled by the caller as a local-override bypass,
    // so it is not normally set here, but honor it if present).
    if mods.contains(KeyModifiers::SHIFT) {
        cb += 4;
    }
    if mods.contains(KeyModifiers::ALT) {
        cb += 8;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        cb += 16;
    }
    let end = if release { 'm' } else { 'M' };
    Some(format!("\x1b[<{cb};{};{}{end}", col + 1, row + 1).into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_ignores_events_from_superseded_connection() {
        let stale = [
            conn::ConnectionEvent {
                generation: 4,
                event: Ev::Down("old connection closed".to_string()),
            },
            conn::ConnectionEvent {
                generation: 5,
                event: Ev::Sessions(Vec::new()),
            },
            conn::ConnectionEvent {
                generation: 6,
                event: Ev::Bytes {
                    name: "active".to_string(),
                    data: b"stale output".to_vec(),
                    snapshot: false,
                },
            },
        ];
        let current = conn::ConnectionEvent {
            generation: 7,
            event: Ev::Up,
        };

        for event in stale {
            assert!(event_for_generation(7, event).is_none());
        }
        assert!(matches!(event_for_generation(7, current), Some(Ev::Up)));
    }

    #[test]
    fn list_race_recognizes_an_external_rename_by_session_identity() {
        let info = |name: &str, pid: u32, created_ms: u64| SessionInfo {
            name: name.to_string(),
            command: "shell".to_string(),
            title: String::new(),
            created_ms,
            idle_ms: 0,
            running: true,
            state: asd_proto::AgentState::Unknown,
            attached_clients: 1,
            pid,
            cols: 80,
            rows: 24,
        };
        let previous = vec![info("old", 42, 100)];
        let renamed = vec![info("new", 42, 100)];
        let replacement = vec![info("new", 42, 101)];

        assert_eq!(
            renamed_active_session(Some("old"), &previous, &renamed),
            Some(("old".to_string(), "new".to_string()))
        );
        assert_eq!(
            renamed_active_session(Some("old"), &previous, &replacement),
            None,
            "a reused pid for a newly created session is not a rename"
        );
    }

    #[test]
    fn running_shimmer_leaves_a_host_output_idle_window() {
        let first_frame = std::time::Instant::now();

        let early = loop_timing(
            first_frame,
            first_frame + Duration::from_millis(30),
            true,
            false,
            None,
        );
        assert!(!early.shimmer_due);
        assert_eq!(early.poll_timeout, Duration::from_millis(30));

        let old_interval = loop_timing(
            first_frame,
            first_frame + Duration::from_millis(150),
            true,
            false,
            None,
        );
        assert!(
            !old_interval.shimmer_due,
            "the former 150 ms interval does not leave Windows Terminal enough time to refresh links"
        );
        assert_eq!(old_interval.poll_timeout, Duration::from_millis(30));

        let just_before = loop_timing(
            first_frame,
            first_frame + Duration::from_millis(499),
            true,
            false,
            None,
        );
        assert!(!just_before.shimmer_due);

        let due = loop_timing(
            first_frame,
            first_frame + Duration::from_millis(500),
            true,
            false,
            None,
        );
        assert!(due.shimmer_due);
        assert_eq!(due.poll_timeout, Duration::from_millis(30));

        let inactive = loop_timing(
            first_frame,
            first_frame + Duration::from_secs(1),
            false,
            false,
            None,
        );
        assert!(!inactive.shimmer_due);

        let fast_path = loop_timing(
            first_frame,
            first_frame + Duration::from_millis(1),
            true,
            true,
            None,
        );
        assert_eq!(fast_path.poll_timeout, Duration::from_millis(5));
    }

    #[test]
    fn wall_clock_redraws_when_the_displayed_second_changes() {
        assert!(!wall_clock_tick_due(1_000, 1_999, true));
        assert!(wall_clock_tick_due(1_999, 2_000, true));
        assert!(!wall_clock_tick_due(2_000, 2_000, true));
        assert!(
            !wall_clock_tick_due(1_999, 2_000, false),
            "a hidden status bar must not generate invisible host writes"
        );
    }

    #[test]
    fn running_session_expires_locally_without_another_list_response() {
        let session = SessionInfo {
            name: "agent".to_string(),
            command: "codex".to_string(),
            title: String::new(),
            created_ms: 0,
            idle_ms: asd_proto::IDLE_SETTLE_MS - 100,
            running: true,
            state: asd_proto::AgentState::Unknown,
            attached_clients: 0,
            pid: 1,
            cols: 80,
            rows: 24,
        };

        assert!(session_running_after(&session, Duration::from_millis(99)));
        assert!(!session_running_after(&session, Duration::from_millis(100)));
        assert!(!session_running_after(&session, Duration::from_millis(500)));
    }

    #[test]
    fn event_poll_wakes_at_the_local_running_expiry() {
        let first_frame = Instant::now();
        let timing = loop_timing(
            first_frame,
            first_frame,
            true,
            false,
            Some(Duration::from_millis(7)),
        );
        assert_eq!(timing.poll_timeout, Duration::from_millis(7));

        let normal_poll_wins = loop_timing(
            first_frame,
            first_frame,
            true,
            false,
            Some(Duration::from_millis(40)),
        );
        assert_eq!(normal_poll_wins.poll_timeout, Duration::from_millis(30));

        let fast_poll_wins = loop_timing(
            first_frame,
            first_frame,
            true,
            true,
            Some(Duration::from_millis(7)),
        );
        assert_eq!(fast_poll_wins.poll_timeout, Duration::from_millis(5));
    }

    #[test]
    fn output_resets_local_running_expiry() {
        let listed_at = Instant::now();
        let session = SessionInfo {
            name: "agent".to_string(),
            command: "codex".to_string(),
            title: String::new(),
            created_ms: 0,
            idle_ms: asd_proto::IDLE_SETTLE_MS - 100,
            running: true,
            state: asd_proto::AgentState::Unknown,
            attached_clients: 0,
            pid: 1,
            cols: 80,
            rows: 24,
        };
        let listed =
            RunningActivity::default().with_list(std::slice::from_ref(&session), listed_at);
        assert!(!listed.is_running("agent", listed_at + Duration::from_millis(100)));

        let output_at = listed_at + Duration::from_millis(10);
        let refreshed = listed.with_output("agent", output_at);
        assert!(refreshed.is_running("agent", listed_at + Duration::from_millis(100)));
        assert!(refreshed.is_running("agent", output_at + Duration::from_millis(1999)));
        assert!(!refreshed.is_running("agent", output_at + Duration::from_millis(2000)));
    }

    #[test]
    fn stale_list_cannot_override_output_observed_locally() {
        let listed_at = Instant::now();
        let idle_session = SessionInfo {
            name: "agent".to_string(),
            command: "codex".to_string(),
            title: String::new(),
            created_ms: 0,
            idle_ms: asd_proto::IDLE_SETTLE_MS,
            running: false,
            state: asd_proto::AgentState::Unknown,
            attached_clients: 0,
            pid: 1,
            cols: 80,
            rows: 24,
        };
        let output_at = listed_at + Duration::from_millis(10);
        let activity = RunningActivity::default().with_output("agent", output_at);
        let merged = activity.with_list(std::slice::from_ref(&idle_session), output_at);

        assert!(merged.is_running("agent", output_at + Duration::from_millis(1999)));
        assert!(!merged.is_running("agent", output_at + Duration::from_millis(2000)));
    }

    #[test]
    fn moved_url_requests_a_full_repaint_once() {
        fn snapshot(lines: &[&str]) -> RenderSnapshot {
            let cols = lines.iter().map(|line| line.len()).max().unwrap_or(0) as u16;
            let cells = lines
                .iter()
                .map(|line| {
                    let mut row: Vec<asd_vt::CellSnapshot> = line
                        .chars()
                        .map(|character| asd_vt::CellSnapshot {
                            grapheme: character.to_string(),
                            ..asd_vt::CellSnapshot::default()
                        })
                        .collect();
                    row.resize(cols as usize, asd_vt::CellSnapshot::default());
                    std::sync::Arc::new(row)
                })
                .collect();
            RenderSnapshot {
                cols,
                rows: lines.len() as u16,
                cells,
                row_dirty: vec![true; lines.len()],
                cursor: asd_vt::CursorSnapshot::default(),
                palette: [asd_vt::Rgb::default(); 256],
                foreground: asd_vt::Rgb::default(),
                background: asd_vt::Rgb::default(),
            }
        }

        let mut state = HostLinkState::default();
        let first = snapshot(&["answer", "http://example.test/task"]);
        assert!(!state.before_frame(Some(&first), Duration::ZERO));

        let same = snapshot(&["answer", "http://example.test/task"]);
        assert!(!state.before_frame(Some(&same), HOST_URL_SCAN_DEBOUNCE));

        let moved = snapshot(&["http://example.test/task", "more output"]);
        assert!(state.before_frame(Some(&moved), Duration::ZERO));

        let moved_again = snapshot(&["more output", "http://example.test/task"]);
        assert!(
            !state.before_frame(Some(&moved_again), Duration::ZERO),
            "continuous movement needs no additional repaint before another quiet interval"
        );

        let mut wrapped_state = HostLinkState::default();
        let wrapped = snapshot(&["http://example.", "test/task"]);
        assert!(!wrapped_state.before_frame(Some(&wrapped), Duration::ZERO));
        assert!(!wrapped_state.before_frame(Some(&wrapped), HOST_URL_SCAN_DEBOUNCE));
        let changed_continuation = snapshot(&["http://example.", "test/other"]);
        assert!(
            wrapped_state.before_frame(Some(&changed_continuation), Duration::ZERO),
            "a changed soft-wrapped URL continuation must invalidate the old host range"
        );
    }

    #[test]
    fn daemon_idle_sample_is_never_resurrected_locally() {
        let session = SessionInfo {
            name: "idle".to_string(),
            command: "bash".to_string(),
            title: String::new(),
            created_ms: 0,
            idle_ms: 0,
            running: false,
            state: asd_proto::AgentState::Unknown,
            attached_clients: 0,
            pid: 1,
            cols: 80,
            rows: 24,
        };

        assert!(!session_running_after(&session, Duration::ZERO));
        let recent_but_idle = SessionInfo {
            idle_ms: asd_proto::IDLE_SETTLE_MS - 1,
            ..session
        };
        assert!(!session_running_after(&recent_but_idle, Duration::ZERO));
    }

    #[test]
    fn paste_bytes_wraps_only_for_a_session_that_asked_for_it() {
        // The session has bracketed paste on: the markers go back on, so the
        // program sees one paste instead of two Enters.
        assert_eq!(
            paste_bytes("echo one\recho two", true),
            b"\x1b[200~echo one\recho two\x1b[201~".to_vec()
        );
        // It does not: markers would arrive as literal text, so send the text
        // alone — line breaks act as Enter, which is all such a program has.
        assert_eq!(
            paste_bytes("echo one\recho two", false),
            b"echo one\recho two".to_vec()
        );
    }

    #[test]
    fn paste_bytes_removes_an_end_marker_inside_the_text() {
        // Pasted text carrying the terminator would end the paste early and
        // the rest would arrive as keystrokes — i.e. pasting a file could run
        // commands. The marker is dropped, the text around it is kept.
        assert_eq!(
            paste_bytes("safe\x1b[201~rm -rf /\r", true),
            b"\x1b[200~saferm -rf /\r\x1b[201~".to_vec()
        );
    }

    #[test]
    fn cursor_tail_places_before_visibility() {
        // Visible pane cursor: CUP (1-based) then show — boo's order, so a
        // frame torn at the tail still never paints the cursor mid-body.
        assert_eq!(cursor_tail(Some((49, 0, true))), b"\x1b[1;50H\x1b[?25h");
        // Hidden-cursor session (pi / Claude Code): positioned for the IME
        // box, but left hidden.
        assert_eq!(cursor_tail(Some((3, 9, false))), b"\x1b[10;4H\x1b[?25l");
        // No cursor this frame (scrolled back / kill modal / no session).
        assert_eq!(cursor_tail(None), b"\x1b[?25l");
    }

    #[test]
    fn frame_buf_is_one_atomic_unit() {
        use std::io::Write;
        let frame = FrameBuf::default();
        frame.begin();
        // The ratatui backend writes the cell diff (and its own cursor-hide)
        // through the shared handle; none of it may flush early.
        let mut backend_handle = frame.clone();
        backend_handle.write_all(b"<cells>\x1b[?25l").unwrap();
        backend_handle.flush().unwrap();
        assert_eq!(
            frame.0.borrow().as_slice(),
            b"\x1b[?2026h\x1b[?25l<cells>\x1b[?25l"
        );
        // begin() starts the next frame from scratch.
        frame.begin();
        assert_eq!(frame.0.borrow().as_slice(), b"\x1b[?2026h\x1b[?25l");
    }

    #[test]
    fn host_link_repaint_stays_on_the_current_host_screen() {
        use std::io::Write;
        let frame = FrameBuf::default();
        frame.begin();
        frame.preserve_host_screen();
        let mut backend_handle = frame.clone();
        backend_handle.write_all(b"<full repaint>").unwrap();

        assert_eq!(
            frame.0.borrow().as_slice(),
            b"\x1b[?2026h\x1b[?25l<full repaint>"
        );
    }

    #[test]
    fn sgr_mouse_encodes_wheel_click_and_modifiers() {
        let sgr = [1000u16, 1006];
        // Wheel up/down: buttons 64/65, 1-based pane-relative coords.
        assert_eq!(
            encode_sgr_mouse(MouseEventKind::ScrollUp, KeyModifiers::NONE, 0, 0, &sgr),
            Some(b"\x1b[<64;1;1M".to_vec())
        );
        assert_eq!(
            encode_sgr_mouse(MouseEventKind::ScrollDown, KeyModifiers::NONE, 4, 2, &sgr),
            Some(b"\x1b[<65;5;3M".to_vec())
        );
        // Left press (M) and release (m).
        assert_eq!(
            encode_sgr_mouse(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE,
                9,
                4,
                &sgr
            ),
            Some(b"\x1b[<0;10;5M".to_vec())
        );
        assert_eq!(
            encode_sgr_mouse(
                MouseEventKind::Up(MouseButton::Left),
                KeyModifiers::NONE,
                9,
                4,
                &sgr
            ),
            Some(b"\x1b[<0;10;5m".to_vec())
        );
        // Ctrl adds 16 to the button code.
        assert_eq!(
            encode_sgr_mouse(MouseEventKind::ScrollUp, KeyModifiers::CONTROL, 0, 0, &sgr),
            Some(b"\x1b[<80;1;1M".to_vec())
        );
    }

    #[test]
    fn sgr_mouse_motion_only_when_the_session_wants_it() {
        // Drag is dropped unless the session enabled 1002/1003.
        assert_eq!(
            encode_sgr_mouse(
                MouseEventKind::Drag(MouseButton::Left),
                KeyModifiers::NONE,
                0,
                0,
                &[1000, 1006]
            ),
            None
        );
        assert_eq!(
            encode_sgr_mouse(
                MouseEventKind::Drag(MouseButton::Left),
                KeyModifiers::NONE,
                0,
                0,
                &[1002, 1006]
            ),
            Some(b"\x1b[<32;1;1M".to_vec())
        );
        // Bare motion needs 1003.
        assert_eq!(
            encode_sgr_mouse(
                MouseEventKind::Moved,
                KeyModifiers::NONE,
                0,
                0,
                &[1002, 1006]
            ),
            None
        );
        assert_eq!(
            encode_sgr_mouse(
                MouseEventKind::Moved,
                KeyModifiers::NONE,
                1,
                1,
                &[1003, 1006]
            ),
            Some(b"\x1b[<35;2;2M".to_vec())
        );
    }
}
