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
//! creates, `x` kills, `r` reconnects, `q` quits, `Ctrl+A` sends a literal
//! Ctrl+A. The mouse selects/kills in the sidebar and scrolls the pane
//! (local scrollback, like `asd attach`); Shift+PageUp/PageDown scroll too.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use asd_proto::SessionInfo;
use asd_vt::{GhosttyVt, Key as VtKey, KeyEvent, Mods, RenderSnapshot, VtBackend};
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent as CtKey, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;

mod conn;
mod key;
mod ui;

use conn::{Cmd, Conn, Ev};

/// Scrollback kept by the local terminal.
const SCROLLBACK: usize = 10_000;
/// Wheel scroll step in lines.
const WHEEL_STEP: usize = 3;

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
    /// Drag selection over the pane, if any.
    sel: Option<Sel>,
    /// True between mouse press and release while dragging a selection.
    selecting: bool,

    pub daemon_up: bool,
    pub notice: Option<String>,
    /// True while waiting for the key after Ctrl+A.
    pub prefix: bool,
    pub now_ms: u64,

    /// Session named on the command line, consumed by the first auto-select.
    preferred: Option<String>,
    /// The previous session's last frame, shown while a switch converges so
    /// the pane never flashes black (double buffering across attaches).
    cache: Option<RenderSnapshot>,
    /// Keep showing `cache` until this deadline; set generously at select
    /// time and tightened once the new Snapshot has been fed and the size
    /// jiggle sent (the convergence window).
    pane_hold: Option<std::time::Instant>,
    /// Sidebar row effects (tachyonfx), keyed by session name: sweep-in on
    /// newly listed sessions, a brief accent fade on selection.
    row_fx: Vec<(String, tachyonfx::Effect)>,
    /// Previous frame instant, for effect timing.
    last_frame: std::time::Instant,
    dirty: bool,
    quit: bool,
}

/// Open the TUI against `socket`; `session` preselects one by name. The
/// daemon must already be running (the `asd ui` wrapper ensures it).
pub fn run(socket: PathBuf, session: Option<String>) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
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
    let grid = ui::pane_grid(ratatui::layout::Rect::new(0, 0, size.width, size.height));

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
        sel: None,
        selecting: false,
        daemon_up: false,
        notice: None,
        prefix: false,
        now_ms: now_ms(),
        preferred,
        cache: None,
        pane_hold: None,
        row_fx: Vec::new(),
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
            terminal.draw(|f| ui::draw(f, &mut app))?;
            // Effects animate frame-by-frame, and a pane hold must expire on
            // time: stay dirty while either is pending (the input poll below
            // caps the frame rate at ~33 fps).
            app.dirty = !app.row_fx.is_empty() || app.pane_hold.is_some();
        }
        if event::poll(Duration::from_millis(30))? {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => app.on_key(k),
                Event::Mouse(m) => app.on_mouse(m, terminal.size()?),
                Event::Paste(text) => {
                    app.scroll = 0;
                    app.send(Cmd::Input(text.into_bytes()));
                }
                Event::Resize(w, h) => {
                    let grid = ui::pane_grid(ratatui::layout::Rect::new(0, 0, w, h));
                    if grid != app.grid {
                        app.grid = grid;
                        if let Some(vt) = &mut app.vt {
                            vt.resize(grid.0, grid.1);
                        }
                        app.send(Cmd::Resize {
                            cols: grid.0,
                            rows: grid.1,
                        });
                    }
                    app.dirty = true;
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

    /// Advance and paint the sidebar row effects; called once per drawn frame.
    pub(crate) fn process_fx(
        &mut self,
        buf: &mut ratatui::buffer::Buffer,
        side: ratatui::layout::Rect,
    ) {
        let now = std::time::Instant::now();
        let delta: tachyonfx::Duration = now.duration_since(self.last_frame).into();
        self.last_frame = now;
        if self.row_fx.is_empty() {
            return;
        }
        let sessions = &self.sessions;
        self.row_fx.retain_mut(|(name, fx)| {
            let Some(i) = sessions.iter().position(|s| &s.name == name) else {
                return false;
            };
            let y = side.top() + (i as u16) * 2;
            if y + 1 >= side.bottom() {
                return false;
            }
            let rect = ratatui::layout::Rect::new(side.left(), y, side.width.saturating_sub(1), 2);
            fx.process(delta, buf, rect);
            !fx.done()
        });
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.conn.cmd_tx.send(cmd);
    }

    /// Current frame of the attached terminal, if any. Re-clamps the scroll
    /// offset first: the scrollback can shrink under it (e.g. the session
    /// entered the alternate screen), and a stale offset would leave the
    /// scroll indicator lying about a view that is actually live.
    pub fn snapshot(&mut self) -> Option<RenderSnapshot> {
        // Across a switch, keep the previous frame up until the new session
        // has converged (snapshot fed + repaint jiggle done).
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
        if let Some(vt) = &mut self.vt {
            self.scroll = self.scroll.min(vt.scrollback_rows());
        }
        let scroll = self.scroll;
        self.vt.as_mut().map(|vt| {
            vt.set_scroll(scroll);
            vt.render_snapshot()
        })
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
        self.active = Some(name.clone());
        // Hold the old session's last frame on screen while the new attach
        // converges — never draw the empty terminal (a black flash).
        self.cache = self.vt.as_mut().map(|vt| {
            vt.set_scroll(0);
            vt.render_snapshot()
        });
        self.pane_hold = Some(std::time::Instant::now() + std::time::Duration::from_millis(400));
        self.vt = Some(GhosttyVt::new(self.grid.0, self.grid.1, SCROLLBACK));
        self.scroll = 0;
        self.sel = None;
        self.selecting = false;
        self.notice = None;
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
        let next = (cur + delta).rem_euclid(n) as usize;
        self.select(self.sessions[next].name.clone());
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
            }
            Ev::Sessions(list) => {
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
                    let pick = self
                        .preferred
                        .take_if(|p| self.sessions.iter().any(|s| &s.name == p))
                        .or_else(|| self.sessions.first().map(|s| s.name.clone()));
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
                if snapshot {
                    // The attach dump carries the daemon Formatter's known
                    // drift (trailing blank rows are dropped), which can leave
                    // stray content on rows the real screen has blank. Nothing
                    // sits BELOW the cursor for shells and inline TUIs on the
                    // real screen (and alt-screen apps repaint fully after the
                    // jiggle below), so erase below it locally.
                    if let Some(vt) = &mut self.vt {
                        let snap = vt.render_snapshot();
                        if let Some((cx, cy)) = snap.cursor.position
                            && cy + 1 < snap.rows
                        {
                            // Move below the cursor, erase to end of screen,
                            // put the cursor back (all local; CSI is 1-based).
                            let seq =
                                format!("\x1b[{};1H\x1b[J\x1b[{};{}H", cy + 2, cy + 1, cx + 1);
                            vt.feed(seq.as_bytes());
                        }
                    }
                    // Then jiggle the pty size so the session app repaints
                    // absolutely — the follow-up Output converges the terminal
                    // (and the cursor) to the truth.
                    let (cols, rows) = self.grid;
                    if rows > 2 {
                        self.send(Cmd::Resize {
                            cols,
                            rows: rows - 1,
                        });
                        self.send(Cmd::Resize { cols, rows });
                    }
                    // The dump is in and the repaint requested: swap to the
                    // live view as soon as the app has had time to redraw.
                    if self.pane_hold.is_some() {
                        self.pane_hold =
                            Some(std::time::Instant::now() + std::time::Duration::from_millis(150));
                    }
                }
            }
            Ev::SessionEnded { name, msg } => {
                if self.active.as_deref() == Some(&name) {
                    self.notice = Some(format!("{name} ended — {msg}"));
                }
            }
        }
        self.dirty = true;
    }

    fn on_key(&mut self, k: CtKey) {
        self.dirty = true;
        let ctrl_a = k.code == KeyCode::Char('a') && k.modifiers.contains(KeyModifiers::CONTROL);

        if self.prefix {
            self.prefix = false;
            match k.code {
                KeyCode::Char('j') | KeyCode::Down => self.select_by_offset(1),
                KeyCode::Char('k') | KeyCode::Up => self.select_by_offset(-1),
                KeyCode::Char(c @ '1'..='9') => {
                    let i = (c as usize) - ('1' as usize);
                    if let Some(s) = self.sessions.get(i) {
                        self.select(s.name.clone());
                    }
                }
                KeyCode::Char('c') => self.send(Cmd::Create),
                KeyCode::Char('x') => {
                    if let Some(name) = self.active.clone() {
                        self.send(Cmd::Kill { name });
                    }
                }
                KeyCode::Char('r') => self.reconnect(),
                KeyCode::Char('q') | KeyCode::Char('d') => self.quit = true,
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

    fn forward(&mut self, ev: KeyEvent) {
        // Typing snaps back to the live bottom and clears any selection.
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
            self.dirty = true;
        }
    }

    fn on_mouse(&mut self, m: MouseEvent, size: ratatui::layout::Size) {
        let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
        let (_, pane, _) = ui::areas(area);
        let in_pane = m.column >= pane.left()
            && m.column < pane.right()
            && m.row >= pane.top()
            && m.row < pane.bottom();
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll_by(WHEEL_STEP as isize),
            MouseEventKind::ScrollDown => self.scroll_by(-(WHEEL_STEP as isize)),
            MouseEventKind::Down(_) => {
                if let Some((i, kill)) = ui::sidebar_hit(area, self.sessions.len(), m.column, m.row)
                {
                    let name = self.sessions[i].name.clone();
                    if kill {
                        self.send(Cmd::Kill { name });
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
                // nothing behind. Screen-space coords are scroll-independent,
                // so the copy captures the whole range even off-view.
                if let Some(sel) = self.sel.take()
                    && sel.anchor != sel.head
                    && let Some(vt) = &mut self.vt
                {
                    let text = vt.selection_text_screen(
                        (sel.anchor.0, sel.anchor.1 as u32),
                        (sel.head.0, sel.head.1 as u32),
                    );
                    if !text.is_empty() {
                        use std::io::Write;
                        let mut out = std::io::stdout();
                        let _ = out.write_all(&asd_vt::clip::osc52_copy(&text));
                        let _ = out.flush();
                    }
                }
                self.dirty = true;
            }
            _ => {}
        }
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

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
