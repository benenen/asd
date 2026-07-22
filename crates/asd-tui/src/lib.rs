//! `asd-tui`: terminal UI client (ratatui) — a session sidebar next to a live
//! terminal pane, switching between the local daemon's sessions (the layout in
//! `images/image.png`). Opened by `asd ui [session]`.
//!
//! Threading: the TUI thread owns the `!Send` [`GhosttyVt`] and ratatui; a
//! background thread ([`conn`]) owns the daemon connection and exchanges plain
//! data over channels — the same split as every other asd client.
//!
//! Keys: everything is forwarded to the attached session except the `Ctrl+A`
//! prefix (screen-style): `j/k` or arrows switch sessions, `1-9` jump, `c`
//! creates, `r` renames (input modal), `x` kills (confirmation modal), `b`/`s`
//! hide the sidebar / bottom status bar (the latter frees the pane's bottom row
//! so an input line can reach the window edge, keeping the IME box from covering
//! it), `R` reconnects, `q` quits, `Ctrl+A` sends a literal Ctrl+A. The mouse
//! selects/kills in the sidebar and scrolls the pane (local scrollback, like
//! `asd attach`) — but when the focused session tracks the mouse (opencode,
//! vim, htop) the event is forwarded to it instead (SGR-encoded); Shift keeps
//! the mouse local, and Shift+PageUp/PageDown scroll too.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use asd_proto::SessionInfo;
use asd_vt::{GhosttyVt, Key as VtKey, KeyEvent, Mods, RenderSnapshot, VtBackend};
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent as CtKey, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};

mod conn;
mod key;
mod modal;
mod ui;

use conn::{Cmd, Conn, Ev};
use modal::{Modal, RenameInput, validate_rename};

/// Scrollback kept by the local terminal.
const SCROLLBACK: usize = 10_000;
/// Wheel scroll step in lines.
const WHEEL_STEP: usize = 3;
/// Longest the pane defers a repaint while a program holds a synchronized-output
/// (`?2026`) update open, bounding a lost `?2026l` (matches typical terminals).
const SYNC_MAX: Duration = Duration::from_millis(150);

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
    ev_rx: Receiver<Ev>,
    ev_tx: Sender<Ev>,

    pub sessions: Vec<SessionInfo>,
    /// The attached session's name.
    pub active: Option<String>,
    /// Local terminal for the attached session (recreated per attach).
    vt: Option<GhosttyVt>,
    /// Local scrollback offset: 0 = follow live output.
    pub scroll: usize,
    /// Terminal grid offered by the pane.
    grid: (u16, u16),
    /// Whole-terminal size (cols, rows), for recomputing the layout on a
    /// sidebar resize/toggle without a `Resize` event.
    term_size: (u16, u16),
    /// Current sidebar width (draggable; [`ui::MIN_SIDEBAR`]..[`ui::MAX_SIDEBAR`]).
    sidebar_w: u16,
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

    pub daemon_up: bool,
    pub notice: Option<String>,
    /// An open modal overlay (rename input or kill confirmation); captures all
    /// keys until it closes.
    pub modal: Option<Modal>,
    /// True while waiting for the key after Ctrl+A.
    pub prefix: bool,
    pub now_ms: u64,

    /// Session named on the command line, consumed by the first auto-select.
    preferred: Option<String>,
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
    /// A continuous color shimmer for each *running* session's row text (its
    /// `running` flag is set — the agent is producing output), keyed by session
    /// name. Added/dropped as the flag toggles; the UI's own host session is
    /// excluded (it always produces output — the TUI itself — so it would
    /// always shimmer).
    running_fx: Vec<(String, tachyonfx::Effect)>,
    /// Previous frame instant, for effect timing.
    last_frame: std::time::Instant,
    dirty: bool,
    quit: bool,
}

/// The cooked termios captured before raw mode, so the signal handler can put
/// the line discipline back. A leaked box, loaded via an async-signal-safe
/// atomic read inside the handler.
static ORIG_TERMIOS: std::sync::atomic::AtomicPtr<libc::termios> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Escape sequences that undo the modes the TUI turns on: disable mouse tracking
/// (SGR 1006/1015 + 1000/1002/1003), bracketed paste (2004), leave the alternate
/// screen (1049), show the cursor (25h), reset SGR (0m). Written verbatim from
/// the signal handler.
const TERM_RESTORE: &[u8] =
    b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?1049l\x1b[?25h\x1b[0m";

/// SIGHUP/SIGTERM/SIGINT handler: a kill or a closed terminal (SSH drop) skips
/// `run`'s normal cleanup, which would leave the terminal in mouse-tracking mode
/// spewing `ESC[<..M` on every mouse move. Restore the terminal, then re-raise
/// the signal with the default disposition so the exit status is unchanged. Only
/// async-signal-safe calls here (write / tcsetattr / signal / raise).
extern "C" fn on_terminating_signal(sig: libc::c_int) {
    unsafe {
        libc::write(
            libc::STDOUT_FILENO,
            TERM_RESTORE.as_ptr().cast(),
            TERM_RESTORE.len(),
        );
        let orig = ORIG_TERMIOS.load(std::sync::atomic::Ordering::SeqCst);
        if !orig.is_null() {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, orig);
        }
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Capture the cooked termios and install the terminal-restore handlers for the
/// signals that would otherwise kill the process without cleanup. Call before
/// entering raw mode.
fn install_terminating_signal_restore() {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut t) == 0 {
            ORIG_TERMIOS.store(
                Box::into_raw(Box::new(t)),
                std::sync::atomic::Ordering::SeqCst,
            );
        }
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_terminating_signal as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        for sig in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT] {
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// Open the TUI against `socket`; `session` preselects one by name. The
/// daemon must already be running (the `asd ui` wrapper ensures it).
pub fn run(socket: PathBuf, session: Option<String>) -> anyhow::Result<()> {
    // Restore the terminal even on a kill / hangup (external `kill`, closed tab,
    // dropped SSH) — a panic hook only fires for Rust panics, not signals. Must
    // run before raw mode so it captures the cooked termios.
    install_terminating_signal_restore();
    let mut terminal = ratatui::init();
    // `ratatui::init` installs a panic hook that restores the screen (leaves the
    // alt screen, disables raw mode) — but it doesn't know about the mouse
    // capture + bracketed paste we enable below. Chain a hook that turns those
    // back off first, so a panic can't leave the terminal in mouse-tracking mode
    // spewing `ESC[<..M` reports on every mouse move (garbage at the shell).
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(
            std::io::stdout(),
            event::DisableBracketedPaste,
            event::DisableMouseCapture
        );
        prev_hook(info);
    }));
    let _ = execute!(
        std::io::stdout(),
        event::EnableMouseCapture,
        event::EnableBracketedPaste
    );

    let result = event_loop(&mut terminal, socket, session);

    let _ = execute!(
        std::io::stdout(),
        event::DisableBracketedPaste,
        event::DisableMouseCapture
    );
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    socket: PathBuf,
    preferred: Option<String>,
) -> anyhow::Result<()> {
    let (ev_tx, ev_rx) = channel::<Ev>();
    let conn = Conn::spawn(socket.clone(), ev_tx.clone());
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
        sessions: Vec::new(),
        active: None,
        vt: None,
        scroll: 0,
        grid,
        term_size: (size.width, size.height),
        sidebar_w,
        sidebar_hidden: false,
        status_hidden: false,
        dragging_divider: false,
        sel: None,
        selecting: false,
        clipboard: None,
        daemon_up: false,
        notice: None,
        modal: None,
        prefix: false,
        now_ms: now_ms(),
        preferred,
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

    while !app.quit {
        while let Ok(ev) = app.ev_rx.try_recv() {
            app.on_conn_event(ev);
        }
        if app.dirty {
            app.now_ms = now_ms();
            // Composite each frame atomically (DEC 2026 synchronized output) so a
            // ~33 fps shimmer redraw can't display a partially-written frame: the
            // sidebar-cell rewrites would otherwise drag the real terminal cursor
            // across them before `ui::draw` repositions it to the pane. We keep
            // the REAL cursor (ui::draw positions it — the IME/codex/vim anchor to
            // it) and, critically, never hide it per frame: the old `hide_cursor()`
            // emitted `?25l`/`?25h` every frame, and that visibility toggle was the
            // flicker. Terminals without 2026 ignore the mode.
            let _ = execute!(std::io::stdout(), BeginSynchronizedUpdate);
            terminal.draw(|f| ui::draw(f, &mut app))?;
            let _ = execute!(std::io::stdout(), EndSynchronizedUpdate);
            // Effects animate frame-by-frame, and a pane hold must expire on
            // time: stay dirty while any is pending (the input poll below caps
            // the frame rate at ~33 fps). The running borders breathe as long
            // as some session is producing output.
            app.dirty =
                !app.row_fx.is_empty() || !app.running_fx.is_empty() || app.pane_hold.is_some();
        }
        // Tighten the loop while a switch converges or effects animate:
        // conn events are only drained between polls, so a long poll adds
        // whole quanta of latency to the dump/repaint pipeline.
        let poll_ms = if app.pane_hold.is_some() || !app.row_fx.is_empty() {
            5
        } else {
            30
        };
        if event::poll(Duration::from_millis(poll_ms))? {
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
                        app.send(Cmd::Input(text.into_bytes()));
                    }
                }
                Event::Resize(w, h) => {
                    app.term_size = (w, h);
                    app.apply_layout();
                }
                _ => {}
            }
        }
    }
    app.send(Cmd::Shutdown);
    Ok(())
}

impl App {
    /// Schedule (or replace) a sidebar effect for a session row.
    fn add_fx(&mut self, name: String, fx: tachyonfx::Effect) {
        self.row_fx.retain(|(n, _)| n != &name);
        self.row_fx.push((name, fx));
    }

    /// Sessions scrolled off the top of the sidebar so the active row stays
    /// visible (see [`ui::sidebar_offset`]). Drawing, effects, and mouse
    /// hit-testing all use this, so a click maps to the row the user sees.
    pub(crate) fn sidebar_offset(&self) -> usize {
        // The sidebar spans the body height (whole height minus the bottom bar).
        let cap = (self.term_size.1.saturating_sub(1) / 2) as usize;
        let active_idx = self
            .active
            .as_deref()
            .and_then(|a| self.sessions.iter().position(|s| s.name == a))
            .unwrap_or(0);
        ui::sidebar_offset(active_idx, self.sessions.len(), cap)
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

    /// Keep one color-shimmer effect per running (non-self) session, then
    /// advance each over its two sidebar rows. The text is drawn (in accent)
    /// by [`ui::draw_sidebar`]; the effect only rotates its hue, via a
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
        if self.active.as_deref() == Some(&name) {
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
                self.vt = None;
                self.pane_cache = None;
            }
            Ev::Sessions(list) => {
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
                // The attached session vanished (killed elsewhere): fall back
                // to the first remaining one.
                if let Some(a) = &self.active
                    && !self.sessions.iter().any(|s| &s.name == a)
                {
                    self.active = None;
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
                if snapshot {
                    // A snapshot is a full redraw into a clean terminal.
                    self.vt = Some(GhosttyVt::new(self.grid.0, self.grid.1, SCROLLBACK));
                    self.scroll = 0;
                    self.sel = None;
                    // The old cache belongs to a different terminal — drop it.
                    self.pane_cache = None;
                    self.sync_since = None;
                }
                if let Some(vt) = &mut self.vt {
                    vt.feed(&data);
                    // Query answers (DA/DSR) must reach the pty or vim-like
                    // programs hang probing.
                    let replies = vt.take_pty_responses();
                    if !replies.is_empty() {
                        self.send(Cmd::Input(replies));
                    }
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
            Ev::Renamed(res) => {
                // Success is already reflected optimistically; the poll that
                // followed the rename confirms it. Surface a rejection.
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
        let ctrl_a = k.code == KeyCode::Char('a') && k.modifiers.contains(KeyModifiers::CONTROL);

        if self.prefix {
            self.prefix = false;
            match k.code {
                KeyCode::Char('j') | KeyCode::Down => self.select_by_offset(1),
                KeyCode::Char('k') | KeyCode::Up => self.select_by_offset(-1),
                KeyCode::Char(c @ '1'..='9') => {
                    if let Some(i) = ui::jump_index(c)
                        && let Some(s) = self.sessions.get(i)
                    {
                        self.select(s.name.clone());
                    }
                }
                KeyCode::Char('c') => self.send(Cmd::Create),
                // Toggle the sidebar; the pane grid + session resize follow.
                KeyCode::Char('b') => {
                    self.sidebar_hidden = !self.sidebar_hidden;
                    self.apply_layout();
                }
                KeyCode::Char('s') => {
                    // Toggle the bottom status bar: hidden gives the pane the
                    // full window height so an input line can reach the true
                    // bottom (keeps the OS IME candidate box from covering it).
                    self.status_hidden = !self.status_hidden;
                    self.apply_layout();
                }
                // Kill asks first (confirmation modal); rename opens an input.
                KeyCode::Char('x') => {
                    if let Some(name) = self.active.clone() {
                        self.modal = Some(Modal::KillConfirm { target: name });
                    }
                }
                KeyCode::Char('r') => {
                    if let Some(name) = self.active.clone() {
                        self.modal = Some(Modal::Rename(RenameInput::new(name)));
                    }
                }
                // Reconnect moved to R (r now renames).
                KeyCode::Char('R') => self.reconnect(),
                // `q` quits; `Esc` (and any other unmapped key) just cancels the
                // prefix. `d` is intentionally not a quit alias — in screen/tmux
                // muscle memory it means detach, which here would read as an
                // accidental quit.
                KeyCode::Char('q') => self.quit = true,
                // Ctrl+A twice sends a literal Ctrl+A to the session.
                KeyCode::Char('a') if ctrl_a => self.forward(KeyEvent {
                    key: VtKey::Char('a'),
                    mods: Mods {
                        ctrl: true,
                        ..Mods::default()
                    },
                    text: Some("a".into()),
                }),
                _ => {} // unknown prefix key: swallow
            }
            return;
        }
        if ctrl_a {
            self.prefix = true;
            return;
        }

        // Shift+PageUp/PageDown drive the local scrollback (like attach).
        if k.modifiers.contains(KeyModifiers::SHIFT) {
            let page = self.grid.1.saturating_sub(1) as usize;
            match k.code {
                KeyCode::PageUp => return self.scroll_by(page as isize),
                KeyCode::PageDown => return self.scroll_by(-(page as isize)),
                _ => {}
            }
        }

        if let Some(ev) = key::map_key(&k) {
            self.forward(ev);
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

    /// Send a rename and optimistically update the local sidebar/active name so
    /// it reflects immediately; the daemon's follow-up `ListSessions` confirms
    /// (or a `Renamed(Err)` reverts it via a notice).
    fn submit_rename(&mut self, target: String, new: String) {
        if target == new {
            return;
        }
        if self.active.as_deref() == Some(&target) {
            self.active = Some(new.clone());
        }
        for s in &mut self.sessions {
            if s.name == target {
                s.name = new.clone();
            }
        }
        self.send(Cmd::Rename {
            name: target,
            new_name: new,
        });
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
            MouseEventKind::ScrollUp => self.scroll_by(WHEEL_STEP as isize),
            MouseEventKind::ScrollDown => self.scroll_by(-(WHEEL_STEP as isize)),
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
            // reach us. Sent as plain input, like a host bracketed paste. (A
            // mouse-tracking session gets the right-click forwarded above; this
            // arm is reached for a plain shell prompt.)
            MouseEventKind::Down(MouseButton::Right) if in_pane => {
                if let Some(text) = self.clipboard.clone() {
                    self.send(Cmd::Input(text.into_bytes()));
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
            }
            self.send(Cmd::Resize {
                cols: grid.0,
                rows: grid.1,
            });
        }
        self.pane_needs_render = true;
        self.dirty = true;
    }

    /// Tear down the old connection actor and start a fresh one.
    fn reconnect(&mut self) {
        self.send(Cmd::Shutdown);
        self.conn = Conn::spawn(self.socket.clone(), self.ev_tx.clone());
        self.notice = None;
        self.active = None;
        self.vt = None;
        self.dirty = true;
    }
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
