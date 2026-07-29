//! The scripting commands `send` / `peek` / `wait` (ported from boo): drive a
//! session, read its rendered screen, or block until it matches or settles —
//! all without an interactive attach, over the v4 name-addressed frames.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::bail;
use asd_proto::{ClientKind, Frame, IDLE_SETTLE_MS, MAX_FRAME_LEN, Scrollback, code};
use asd_vt::{GhosttyVt, VtBackend};
use tokio::io::AsyncReadExt;

use crate::client;
use crate::exit;

/// Poll interval for `wait` (matches boo).
const POLL_MS: u64 = 50;

/// `asd send`: type bytes into a session's pty. The payload is `--text`
/// (literal), `--key` (named keys), or stdin; `--enter` appends a carriage
/// return. Nothing is escaped, so there is no quoting layer to fight.
pub async fn send(
    socket: &Path,
    name: String,
    text: Option<String>,
    key: Option<String>,
    enter: bool,
    stdin: bool,
) -> anyhow::Result<()> {
    // clap enforces the --text/--key/--stdin exclusivity; build the payload.
    let mut payload: Vec<u8> = if let Some(t) = text {
        t.into_bytes()
    } else if let Some(list) = key {
        let mut out = Vec::new();
        for key_name in list.split(',').filter(|k| !k.is_empty()) {
            match named_key(key_name) {
                Some(bytes) => out.extend_from_slice(&bytes),
                None => bail!("send: unknown key '{key_name}'"),
            }
        }
        out
    } else {
        let _ = stdin; // presence only forces this branch; reading is the default
        // Reading a terminal blocks until Ctrl-D, and this happens before the
        // daemon is even contacted — so without a word `asd send --stdin` at a
        // prompt looks hung rather than waiting.
        if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            eprintln!(
                "send: reading the payload from stdin — type it, then Ctrl-D (or pipe it in)"
            );
        }
        let mut buf = Vec::new();
        tokio::io::stdin().read_to_end(&mut buf).await?;
        if buf.len() > MAX_FRAME_LEN - 1024 {
            bail!("send: stdin too large (max ~4 MiB)");
        }
        buf
    };
    if enter {
        payload = with_enter(payload);
    }
    if payload.is_empty() {
        bail!("send: nothing to send");
    }
    if payload.contains(&0) {
        bail!("send: cannot send NUL bytes");
    }

    let mut c = client::connect(socket, ClientKind::Cli).await?;
    c.writer
        .write_frame(&Frame::SendInput {
            name,
            bytes: payload,
        })
        .await?;
    match c.reader.read_frame().await? {
        Some(Frame::Ack) => Ok(()),
        Some(Frame::Error { code, msg }) => Err(exit::daemon("send", code, &msg)),
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// `asd peek`: print a session's rendered screen, optionally with history above
/// it, as plain text or as a JSON object.
pub async fn peek(
    socket: &Path,
    name: String,
    scrollback: Scrollback,
    json: bool,
) -> anyhow::Result<()> {
    let mut c = client::connect(socket, ClientKind::Cli).await?;
    c.writer
        .write_frame(&Frame::Peek {
            name: name.clone(),
            scrollback,
        })
        .await?;
    let reply = match c.reader.read_frame().await? {
        Some(f @ Frame::PeekReply { .. }) => f,
        Some(Frame::Error { code, msg }) => return Err(exit::daemon("peek", code, &msg)),
        other => bail!("unexpected reply: {other:?}"),
    };
    let Frame::PeekReply {
        cols,
        rows,
        cursor_col,
        cursor_row,
        title,
        screen,
    } = reply
    else {
        unreachable!("matched PeekReply above")
    };

    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    if !json {
        out.write_all(&screen)?;
        // Guarantee a trailing newline so the screen is a clean text block.
        if screen.last() != Some(&b'\n') {
            out.write_all(b"\n")?;
        }
        return Ok(());
    }

    let mut s = String::from(r#"{"session":"#);
    json_string(&name, &mut s);
    s.push_str(r#","title":"#);
    json_string(&title, &mut s);
    s.push_str(&format!(
        r#","rows":{rows},"cols":{cols},"cursor":{{"row":{cursor_row},"col":{cursor_col}}},"screen":"#
    ));
    json_string(&String::from_utf8_lossy(&screen), &mut s);
    s.push_str("}\n");
    out.write_all(s.as_bytes())?;
    Ok(())
}

/// `asd inspect`: print everything the daemon knows about one session —
/// metadata plus live terminal internals — as a labeled block or, with
/// `--json`, a machine-readable object.
pub async fn inspect(socket: &Path, name: String, json: bool) -> anyhow::Result<()> {
    let mut c = client::connect(socket, ClientKind::Cli).await?;
    c.writer.write_frame(&Frame::Inspect { name }).await?;
    let reply = match c.reader.read_frame().await? {
        Some(f @ Frame::InspectReply { .. }) => f,
        Some(Frame::Error { code, msg }) => return Err(exit::daemon("inspect", code, &msg)),
        other => bail!("unexpected reply: {other:?}"),
    };
    let Frame::InspectReply {
        info,
        child_pid,
        alt_screen,
        scrollback_rows,
        mouse_tracking,
        mouse_modes,
        cursor_col,
        cursor_row,
        cursor_visible,
    } = reply
    else {
        unreachable!("matched InspectReply above")
    };

    let status = if info.running { "running" } else { "idle" };
    let screen = if alt_screen { "alternate" } else { "primary" };
    let mouse = if mouse_tracking {
        format!("on {mouse_modes:?}")
    } else {
        "off".to_string()
    };
    let cursor_vis = if cursor_visible { "visible" } else { "hidden" };

    if json {
        let mut s = String::from(r#"{"session":"#);
        json_string(&info.name, &mut s);
        s.push_str(r#","status":"#);
        json_string(status, &mut s);
        s.push_str(r#","command":"#);
        json_string(&info.command, &mut s);
        s.push_str(r#","title":"#);
        json_string(&info.title, &mut s);
        let modes = mouse_modes
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",");
        s.push_str(&format!(
            r#","pid":{child_pid},"cols":{cols},"rows":{rows},"created_ms":{created},"idle_ms":{idle},"running":{running},"attached_clients":{clients},"alt_screen":{alt},"scrollback_rows":{sb},"mouse_tracking":{mt},"mouse_modes":[{modes}],"cursor":{{"col":{cc},"row":{cr},"visible":{cv}}}}}"#,
            cols = info.cols,
            rows = info.rows,
            created = info.created_ms,
            idle = info.idle_ms,
            running = info.running,
            clients = info.attached_clients,
            alt = alt_screen,
            sb = scrollback_rows,
            mt = mouse_tracking,
            cc = cursor_col,
            cr = cursor_row,
            cv = cursor_visible,
        ));
        s.push('\n');
        print!("{s}");
        return Ok(());
    }

    // Labeled block; `title` only prints when the session set one.
    let mut out = format!(
        "session    {name}\n\
         status     {status}\n\
         command    {command}\n",
        name = info.name,
        command = info.command,
    );
    if !info.title.trim().is_empty() {
        out.push_str(&format!("title      {}\n", info.title));
    }
    out.push_str(&format!(
        "pid        {pid}\n\
         size       {cols}x{rows}\n\
         created    {created}\n\
         idle       {idle}\n\
         clients    {clients}\n\
         screen     {screen}\n\
         scrollback {sb} lines\n\
         mouse      {mouse}\n\
         cursor     ({cc}, {cr}) {cursor_vis}\n",
        pid = child_pid,
        cols = info.cols,
        rows = info.rows,
        created = crate::format_age(info.created_ms),
        idle = fmt_idle(info.idle_ms),
        clients = info.attached_clients,
        sb = scrollback_rows,
        cc = cursor_col,
        cr = cursor_row,
    ));
    print!("{out}");
    Ok(())
}

/// Format an idle duration compactly: `420ms` under a second, else `3.4s`.
fn fmt_idle(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// `asd wait`: block until the session's screen contains `text`, or output has
/// been idle for [`IDLE_SETTLE_MS`], then exit 0. On timeout, exit
/// [`exit::TIMEOUT`]. One persistent connection is polled every [`POLL_MS`].
pub async fn wait(
    socket: &Path,
    name: String,
    text: Option<String>,
    idle: bool,
    timeout: String,
) -> anyhow::Result<()> {
    let _ = idle; // clap guarantees exactly one of --text / --idle
    let timeout_ms = parse_duration(&timeout).ok_or_else(|| {
        anyhow::anyhow!("wait: bad duration '{timeout}' (use 500ms, 2s, 1m, 4h, 1d)")
    })?;

    let mut c = client::connect(socket, ClientKind::Cli).await?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Some(needle) = &text {
            c.writer
                .write_frame(&Frame::Peek {
                    name: name.clone(),
                    scrollback: Scrollback::None,
                })
                .await?;
            match c.reader.read_frame().await? {
                Some(Frame::PeekReply { screen, .. }) => {
                    if String::from_utf8_lossy(&screen).contains(needle.as_str()) {
                        return Ok(());
                    }
                }
                Some(Frame::Error { code, msg }) => return Err(exit::daemon("wait", code, &msg)),
                other => bail!("unexpected reply: {other:?}"),
            }
        } else {
            c.writer.write_frame(&Frame::ListSessions).await?;
            match c.reader.read_frame().await? {
                Some(Frame::SessionList { sessions }) => {
                    match sessions.iter().find(|s| s.name == name) {
                        Some(s) if s.idle_ms >= IDLE_SETTLE_MS => return Ok(()),
                        Some(_) => {}
                        // `ListSessions` cannot fail on a missing name — it just
                        // returns a list without it — so the absence is detected
                        // here. Report it exactly as the daemon would have, code
                        // and wording included: `--text` reaches the same
                        // condition through `Peek`, which *does* answer with
                        // `Error{NO_SUCH_SESSION}`, and one command must not
                        // describe one situation two ways.
                        None => {
                            return Err(exit::daemon(
                                "wait",
                                code::NO_SUCH_SESSION,
                                &format!("no such session '{name}'"),
                            ));
                        }
                    }
                }
                Some(Frame::Error { code, msg }) => return Err(exit::daemon("wait", code, &msg)),
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        if Instant::now() >= deadline {
            eprintln!("wait: timed out after {timeout}");
            std::process::exit(exit::TIMEOUT);
        }
        tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
    }
}

/// One `asd follow --json` event. The stream is a log, so every line says what
/// happened; a consumer that only wants the text filters on `"event":"output"`.
pub(crate) enum FollowEvent<'a> {
    /// A batch of pty output, decoded to text.
    Output(&'a str),
    /// The session's activity flipped (or the opening status on subscribe).
    Status { running: bool, idle_ms: u64 },
    /// The live screen, as it stands where the stream pauses (settle, end,
    /// timeout). This is the part a repaint keeps rewriting, so it is reported
    /// once per pause instead of once per frame.
    Screen(&'a str),
    /// The session ended, or the daemon hung up: end of stream.
    Exit,
    /// `--timeout` expired with the stream still open.
    Timeout,
}

/// Render one event as a JSONL line (no trailing newline). `time_ms` is the
/// client's wall clock at receipt — the daemon does not stamp frames, and a
/// log without time is hard to correlate with anything else.
pub(crate) fn follow_event_json(ev: &FollowEvent<'_>, time_ms: u64) -> String {
    let mut s = String::from(r#"{"event":""#);
    match ev {
        FollowEvent::Output(_) => s.push_str("output"),
        FollowEvent::Status { .. } => s.push_str("status"),
        FollowEvent::Screen(_) => s.push_str("screen"),
        FollowEvent::Exit => s.push_str("exit"),
        FollowEvent::Timeout => s.push_str("timeout"),
    }
    s.push_str(&format!(r#"","time_ms":{time_ms}"#));
    match ev {
        FollowEvent::Output(text) | FollowEvent::Screen(text) => {
            s.push_str(r#","text":"#);
            json_string(text, &mut s);
        }
        FollowEvent::Status { running, idle_ms } => {
            s.push_str(&format!(r#","running":{running},"idle_ms":{idle_ms}"#));
        }
        FollowEvent::Exit | FollowEvent::Timeout => {}
    }
    s.push('}');
    s
}

/// Decodes a byte stream to text across chunk boundaries. Pty batches are cut
/// at arbitrary byte offsets, so a multi-byte character can straddle two
/// `Output` frames; the incomplete tail is held back and prepended to the next
/// chunk instead of being mangled into a replacement char. Bytes that are
/// genuinely not UTF-8 (a session dumping binary) do become U+FFFD.
#[derive(Default)]
pub(crate) struct Utf8Stream {
    tail: Vec<u8>,
}

impl Utf8Stream {
    /// Decode what is complete; keep any partial character for the next call.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> String {
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(bytes);
        let mut out = String::new();
        let mut rest = &buf[..];
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let good = e.valid_up_to();
                    out.push_str(std::str::from_utf8(&rest[..good]).unwrap_or_default());
                    match e.error_len() {
                        // Invalid byte(s) mid-buffer: replace and keep going.
                        Some(n) => {
                            out.push('\u{fffd}');
                            rest = &rest[good + n..];
                        }
                        // Truncated character at the end: hold it back.
                        None => {
                            rest = &rest[good..];
                            break;
                        }
                    }
                }
            }
        }
        self.tail = rest.to_vec();
        out
    }

    /// End of stream: nothing more is coming, so a held-back tail can only be
    /// broken. Emit it lossily rather than swallowing it.
    pub(crate) fn flush(&mut self) -> String {
        let tail = std::mem::take(&mut self.tail);
        String::from_utf8_lossy(&tail).into_owned()
    }
}

/// A local terminal that tells apart what a program *printed* from what it is
/// *repainting*.
///
/// This is the question the byte stream cannot answer. A TUI rewrites its
/// status line several times a second — `✻ building…`, `✽ building… 2`, `·
/// building…` — and to the stream those look exactly like new output. Stripping
/// escape sequences does not help: the sequences *are* the distinction.
///
/// A terminal knows, because it has row identity. Feed the same bytes into a
/// `GhosttyVt` (the one `attach` already renders with) and screen space splits
/// in two: rows below `scrollback_rows()` have scrolled off the live screen and
/// can never be touched again, and the rows above it are the live screen, which
/// a repaint rewrites in place. So a row leaving the screen *is* the signal
/// that its content is final — that is what gets logged as `output`, in order,
/// exactly once. The live screen is reported separately as `screen`, once per
/// pause, no matter how many times it was painted.
///
/// Two consequences worth knowing. Output that never scrolls (a short command
/// on a screen with room to spare) is not final until the session settles, so
/// it arrives in the `screen` event rather than streaming line by line. And a
/// full-screen program on the alternate screen (vim, htop, less) commits
/// nothing at all by design — its screen *is* the content, so `screen` is the
/// only thing to report for it.
pub(crate) struct ScreenModel {
    vt: GhosttyVt,
    /// Screen-space rows already reported as final.
    emitted: usize,
}

impl ScreenModel {
    /// `cols`/`rows` should match the session's pty, or wrapping — and so the
    /// line boundaries this whole thing is built on — will not match either.
    pub(crate) fn new(cols: u16, rows: u16) -> Self {
        Self {
            // Same depth `attach` gives its client-side terminal: the rows are
            // drained after every batch, so this is headroom, not a buffer.
            vt: GhosttyVt::new(cols.max(1), rows.max(1), 100_000),
            emitted: 0,
        }
    }

    /// Feed one pty batch; return the lines that just became final (empty when
    /// the batch only repainted the live screen).
    pub(crate) fn push(&mut self, bytes: &[u8]) -> String {
        self.vt.feed(bytes);
        // A follower has no pty to write to, so the terminal's query replies
        // (DA, DSR) have nowhere to go; drop them rather than accumulate.
        let _ = self.vt.take_pty_responses();
        let committed = self.vt.scrollback_rows();
        if committed <= self.emitted {
            return String::new();
        }
        let lines = self
            .vt
            .fetch_history(self.emitted as u32, (committed - self.emitted) as u32);
        self.emitted = committed;
        join_lines(&lines)
    }

    /// The live screen, trailing blank rows removed (they are padding to the
    /// terminal's height, not content).
    pub(crate) fn screen(&mut self) -> String {
        let start = self.vt.scrollback_rows();
        let total = self.vt.history_len();
        let lines = self
            .vt
            .fetch_history(start as u32, total.saturating_sub(start) as u32);
        let end = lines
            .iter()
            .rposition(|l| !l.is_empty())
            .map_or(0, |i| i + 1);
        join_lines(&lines[..end])
    }
}

/// Rows as one block of text. `fetch_history` already trims each row's trailing
/// blanks, and its bytes come from the terminal's own cells, so they are UTF-8
/// unless a cell held something unrepresentable.
fn join_lines(lines: &[Vec<u8>]) -> String {
    lines
        .iter()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// How `follow` turns pty bytes into event text: verbatim (`--raw`, and always
/// in text mode), or through a terminal model that separates final output from
/// repaints.
pub(crate) enum Decode {
    Raw(Utf8Stream),
    Screen(Box<ScreenModel>),
}

impl Decode {
    fn push(&mut self, bytes: &[u8]) -> String {
        match self {
            Self::Raw(d) => d.push(bytes),
            Self::Screen(m) => m.push(bytes),
        }
    }

    /// End of stream: whatever the decoder was still holding. The screen model
    /// holds nothing — its pending content is the live screen, reported as its
    /// own event.
    fn flush(&mut self) -> String {
        match self {
            Self::Raw(d) => d.flush(),
            Self::Screen(_) => String::new(),
        }
    }

    /// The live screen, when there is a terminal model to ask.
    fn screen(&mut self) -> Option<String> {
        match self {
            Self::Raw(_) => None,
            Self::Screen(m) => Some(m.screen()),
        }
    }
}

/// The session's pty size from the daemon's list, or `None` if it has no such
/// session (which `Follow` will then report properly).
async fn session_size(c: &mut client::Client, name: &str) -> anyhow::Result<Option<(u16, u16)>> {
    c.writer.write_frame(&Frame::ListSessions).await?;
    match c.reader.read_frame().await? {
        Some(Frame::SessionList { sessions }) => Ok(sessions
            .iter()
            .find(|s| s.name == name)
            .map(|s| (s.cols, s.rows))),
        Some(Frame::Error { code, msg }) => Err(exit::daemon("follow", code, &msg)),
        other => bail!("unexpected reply: {other:?}"),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `asd follow`: stream a session's output as the pty produces it, and return
/// when the session settles.
///
/// The stop condition is the daemon's own quiescence signal — the same
/// `idle_ms < IDLE_SETTLE_MS` rule behind `SessionInfo.running` and `asd wait
/// --idle` — rather than a text match. For a Claude Code or Codex session there
/// is no reliable string to match on: the screen is redrawn continuously, so
/// spinners, cursor moves and colour resets all look like progress.
///
/// The status rides the same connection as the output and is produced by the
/// session's own thread, so "these bytes, and now it is quiet" arrives in that
/// order. Nothing polls.
///
/// `until_idle` is the default; `follow --forever` clears it to stream across
/// quiet spells, and then only the session ending (or `--timeout`) stops it.
///
/// `json` turns the stream into JSONL — one event object per line. The daemon
/// sends a `FollowStatus` after *every* batch, so only the transitions are
/// logged; a line-per-batch would bury the output.
///
/// By default the JSONL is modelled rather than stripped ([`ScreenModel`]):
/// bytes go through a local terminal, `output` reports the lines that scrolled
/// off the live screen (final, in order, once), and `screen` reports the live
/// screen at each pause — so a repainting status line is one event per pause
/// instead of ten per second. `raw` skips the model and reports the verbatim
/// stream, escapes and all. Text mode is always verbatim: it is meant for a
/// terminal, which needs them.
pub async fn follow(
    socket: &Path,
    name: String,
    until_idle: bool,
    timeout: Option<String>,
    json: bool,
    raw: bool,
) -> anyhow::Result<()> {
    use std::io::Write as _;

    let deadline = match &timeout {
        Some(t) => Some(
            Instant::now()
                + Duration::from_millis(parse_duration(t).ok_or_else(|| {
                    anyhow::anyhow!("follow: bad duration '{t}' (use 500ms, 2s, 1m, 4h, 1d)")
                })?),
        ),
        None => None,
    };

    let mut c = client::connect(socket, ClientKind::Cli).await?;

    // The terminal model has to be the session's size or its lines wrap
    // somewhere else than the session's do. Ask before subscribing; a name the
    // daemon does not know is left to `Follow` to reject, so the missing-session
    // exit code stays where it is.
    let mut decoder = if json && !raw {
        let (cols, rows) = session_size(&mut c, &name).await?.unwrap_or((80, 24));
        Decode::Screen(Box::new(ScreenModel::new(cols, rows)))
    } else {
        Decode::Raw(Utf8Stream::default())
    };

    c.writer
        .write_frame(&Frame::Follow { name: name.clone() })
        .await?;

    let mut out = std::io::stdout();
    let mut last_running: Option<bool> = None;
    // The live screen is reported at every pause, but only when it has changed
    // since the last report — settle-then-exit would otherwise print it twice.
    let mut last_screen = String::new();
    // Every write is flushed: a follower that buffered would defeat the point
    // of streaming, in either format.
    let emit = |ev: &FollowEvent<'_>, out: &mut std::io::Stdout| -> std::io::Result<()> {
        match ev {
            _ if json => writeln!(out, "{}", follow_event_json(ev, now_ms()))?,
            FollowEvent::Output(text) => out.write_all(text.as_bytes())?,
            // Text mode is the raw stream; status/screen/exit are its control
            // plane and stay invisible.
            _ => {}
        }
        out.flush()
    };
    loop {
        let frame = match deadline {
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                match tokio::time::timeout(left, c.reader.read_frame()).await {
                    Ok(frame) => frame?,
                    // Abandoning a half-read frame is safe here and only here:
                    // the process ends on the next line, so the reader is never
                    // used again. (`read_frame` is not cancel-safe, which is
                    // why it is never put in a `select!`.)
                    Err(_) => {
                        let t = timeout.as_deref().unwrap_or_default();
                        let tail = decoder.flush();
                        if !tail.is_empty() {
                            let _ = emit(&FollowEvent::Output(&tail), &mut out);
                        }
                        if let Some(screen) = decoder.screen()
                            && screen != last_screen
                            && !screen.is_empty()
                        {
                            let _ = emit(&FollowEvent::Screen(&screen), &mut out);
                        }
                        let _ = emit(&FollowEvent::Timeout, &mut out);
                        eprintln!("follow: timed out after {t}");
                        let _ = out.flush();
                        std::process::exit(exit::TIMEOUT);
                    }
                }
            }
            None => c.reader.read_frame().await?,
        };
        match frame {
            Some(Frame::Output { bytes }) => {
                let text = decoder.push(&bytes);
                // A batch can be nothing but the front half of a character, or
                // nothing but a cursor move once cleaned; there is no event to
                // report until something printable arrives.
                if !text.is_empty() {
                    emit(&FollowEvent::Output(&text), &mut out)?;
                }
            }
            Some(Frame::FollowStatus { running, idle_ms }) => {
                // Going quiet is the moment the live screen is worth reporting:
                // whatever was being repainted has stopped moving.
                if !running
                    && let Some(screen) = decoder.screen()
                    && screen != last_screen
                    && !screen.is_empty()
                {
                    emit(&FollowEvent::Screen(&screen), &mut out)?;
                    last_screen = screen;
                }
                if last_running != Some(running) {
                    last_running = Some(running);
                    emit(&FollowEvent::Status { running, idle_ms }, &mut out)?;
                }
                if until_idle && !running {
                    return Ok(());
                }
            }
            // The session ended under us. That is the end of the stream, not a
            // failure of the command: whatever was being waited for is over.
            Some(Frame::Error { code, .. }) if code == code::SESSION_EXITED => {
                let tail = decoder.flush();
                if !tail.is_empty() {
                    emit(&FollowEvent::Output(&tail), &mut out)?;
                }
                if let Some(screen) = decoder.screen()
                    && screen != last_screen
                    && !screen.is_empty()
                {
                    emit(&FollowEvent::Screen(&screen), &mut out)?;
                }
                emit(&FollowEvent::Exit, &mut out)?;
                return Ok(());
            }
            Some(Frame::Error { code, msg }) => return Err(exit::daemon("follow", code, &msg)),
            // The daemon hung up.
            None => {
                let tail = decoder.flush();
                if !tail.is_empty() {
                    emit(&FollowEvent::Output(&tail), &mut out)?;
                }
                if let Some(screen) = decoder.screen()
                    && screen != last_screen
                    && !screen.is_empty()
                {
                    emit(&FollowEvent::Screen(&screen), &mut out)?;
                }
                emit(&FollowEvent::Exit, &mut out)?;
                return Ok(());
            }
            other => bail!("unexpected reply: {other:?}"),
        }
    }
}

/// Append the Enter keypress `--enter` asks for, absorbing a line ending the
/// payload already carried.
///
/// `echo x | asd send s --stdin --enter` would otherwise put `x\n\r` on the
/// pty: `echo` supplies the newline, `--enter` the carriage return. A shell
/// does not care (its line discipline maps both), but a program reading raw
/// input — Claude Code, an editor, anything with its own key handling — reads
/// LF as "insert a line break" and CR as "submit", so the text arrives with a
/// stray newline in it and may not be submitted at all.
///
/// The newline in that payload came from the shell, not from the person typing
/// the command; `--enter` is them saying "and then press Enter". So one
/// trailing line ending (LF or CRLF) folds into the Enter. Without `--enter`
/// nothing is touched — `send` stays a byte-exact pipe.
pub(crate) fn with_enter(mut payload: Vec<u8>) -> Vec<u8> {
    if payload.last() == Some(&b'\n') {
        payload.pop();
        // CRLF is one line ending, not two.
        if payload.last() == Some(&b'\r') {
            payload.pop();
        }
    }
    payload.push(b'\r');
    payload
}

/// What `--scrollback` meant on the command line. clap gives three states for
/// an optionally-valued flag, and they map straight onto the wire type: absent
/// is the screen alone, bare is the whole history, and a value caps it.
pub(crate) fn scrollback_arg(flag: Option<Option<u32>>) -> Scrollback {
    match flag {
        None => Scrollback::None,
        Some(None) => Scrollback::All,
        Some(Some(n)) => Scrollback::Lines(n),
    }
}

/// The byte sequence for a named key (`--key`), or `None` if unrecognized.
/// Arrow/Home/End use the legacy `ESC [ …` forms (same bytes as boo).
fn named_key(name: &str) -> Option<Vec<u8>> {
    let eqi = |a: &str| name.eq_ignore_ascii_case(a);
    let bytes: &[u8] = if eqi("enter") {
        b"\r"
    } else if eqi("tab") {
        b"\t"
    } else if eqi("escape") || eqi("esc") {
        b"\x1b"
    } else if eqi("space") {
        b" "
    } else if eqi("backspace") || eqi("bs") {
        b"\x7f"
    } else if eqi("up") {
        b"\x1b[A"
    } else if eqi("down") {
        b"\x1b[B"
    } else if eqi("right") {
        b"\x1b[C"
    } else if eqi("left") {
        b"\x1b[D"
    } else if eqi("home") {
        b"\x1b[H"
    } else if eqi("end") {
        b"\x1b[F"
    } else {
        // C-a .. C-z → control byte 0x01 .. 0x1a.
        let b = name.as_bytes();
        if b.len() == 3
            && (b[0] == b'C' || b[0] == b'c')
            && b[1] == b'-'
            && b[2].is_ascii_alphabetic()
        {
            return Some(vec![b[2].to_ascii_lowercase() - b'a' + 1]);
        }
        return None;
    };
    Some(bytes.to_vec())
}

/// Parse a duration like `500ms`, `2s`, `1m`, `4h` / `4hr`, `1d` into
/// milliseconds. Requires a unit; `None` on any malformed input.
fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    if split == 0 {
        return None; // no leading integer
    }
    let value: u64 = s[..split].parse().ok()?;
    let mult: u64 = match &s[split..] {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" | "hr" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    value.checked_mul(mult)
}

/// The session list as a JSON array, one object per session — always an array,
/// so `[]` is the empty case rather than a special form.
///
/// Field names match what `inspect --json` emits for the same data (`session`
/// for the name, `status` alongside the `running` bool), so a caller can read
/// either without special-casing. The fields `inspect` adds beyond this are the
/// ones only a session thread can answer (pid, alt-screen, cursor, …); they are
/// absent here rather than null, because `list` never asks for them.
pub fn sessions_json(sessions: &[asd_proto::SessionInfo]) -> String {
    let mut s = String::from("[");
    for (i, info) in sessions.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(r#"{"session":"#);
        json_string(&info.name, &mut s);
        s.push_str(r#","status":"#);
        json_string(if info.running { "running" } else { "idle" }, &mut s);
        s.push_str(r#","command":"#);
        json_string(&info.command, &mut s);
        s.push_str(r#","title":"#);
        json_string(&info.title, &mut s);
        s.push_str(&format!(
            r#","pid":{pid},"cols":{cols},"rows":{rows},"created_ms":{created},"idle_ms":{idle},"running":{running},"attached_clients":{clients}}}"#,
            pid = info.pid,
            cols = info.cols,
            rows = info.rows,
            created = info.created_ms,
            idle = info.idle_ms,
            running = info.running,
            clients = info.attached_clients,
        ));
    }
    s.push(']');
    s
}

/// Append `value` as a JSON-escaped string literal (with surrounding quotes)
/// to `out`. Avoids a serde_json dependency for one small object.
fn json_string(value: &str, out: &mut String) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, running: bool) -> asd_proto::SessionInfo {
        asd_proto::SessionInfo {
            name: name.to_string(),
            command: "bash".to_string(),
            title: String::new(),
            created_ms: 1_700_000_000_000,
            idle_ms: 42,
            running,
            attached_clients: 1,
            pid: 4242,
            cols: 80,
            rows: 24,
        }
    }

    #[test]
    fn sessions_json_carries_the_pid() {
        // The whole point: a caller can reach /proc/<pid> straight from `list`,
        // without an `inspect` round trip per session.
        assert!(sessions_json(&[info("s0", true)]).contains(r#""pid":4242"#));
    }

    #[test]
    fn sessions_json_is_always_an_array() {
        assert_eq!(sessions_json(&[]), "[]");
        let one = sessions_json(&[info("s0", true)]);
        assert!(one.starts_with('[') && one.ends_with(']'), "{one}");
        assert_eq!(one.matches("\"session\"").count(), 1);
    }

    #[test]
    fn sessions_json_reports_status_and_running_together() {
        let out = sessions_json(&[info("a", true), info("b", false)]);
        // Two objects, comma-separated at the top level.
        assert_eq!(out.matches("{\"session\":").count(), 2);
        assert!(
            out.contains(r#"{"session":"a","status":"running""#),
            "{out}"
        );
        assert!(out.contains(r#"{"session":"b","status":"idle""#), "{out}");
        // `status` is the string form of the same flag `running` carries.
        assert!(out.contains(r#""running":true"#), "{out}");
        assert!(out.contains(r#""running":false"#), "{out}");
    }

    #[test]
    fn sessions_json_escapes_names_and_titles() {
        let mut s = info("weird", true);
        s.title = "say \"hi\"\n\tdone\\".to_string();
        let out = sessions_json(&[s]);
        assert!(
            out.contains(r#""title":"say \"hi\"\n\tdone\\""#),
            "title not escaped: {out}"
        );
    }

    #[test]
    fn named_key_covers_documented_names() {
        assert_eq!(named_key("Enter").unwrap(), b"\r");
        assert_eq!(named_key("tab").unwrap(), b"\t");
        assert_eq!(named_key("ESC").unwrap(), b"\x1b");
        assert_eq!(named_key("escape").unwrap(), b"\x1b");
        assert_eq!(named_key("space").unwrap(), b" ");
        assert_eq!(named_key("Backspace").unwrap(), b"\x7f");
        assert_eq!(named_key("bs").unwrap(), b"\x7f");
        assert_eq!(named_key("Up").unwrap(), b"\x1b[A");
        assert_eq!(named_key("down").unwrap(), b"\x1b[B");
        assert_eq!(named_key("Right").unwrap(), b"\x1b[C");
        assert_eq!(named_key("left").unwrap(), b"\x1b[D");
        assert_eq!(named_key("Home").unwrap(), b"\x1b[H");
        assert_eq!(named_key("End").unwrap(), b"\x1b[F");
    }

    #[test]
    fn named_key_control_letters() {
        assert_eq!(named_key("C-a").unwrap(), vec![0x01]);
        assert_eq!(named_key("c-c").unwrap(), vec![0x03]);
        assert_eq!(named_key("C-z").unwrap(), vec![0x1a]);
    }

    #[test]
    fn named_key_rejects_unknown() {
        assert!(named_key("").is_none());
        assert!(named_key("Foo").is_none());
        assert!(named_key("C-1").is_none());
        assert!(named_key("C-ab").is_none());
        assert!(named_key("ctrl-a").is_none());
    }

    #[test]
    fn fmt_idle_compacts() {
        assert_eq!(fmt_idle(0), "0ms");
        assert_eq!(fmt_idle(420), "420ms");
        assert_eq!(fmt_idle(999), "999ms");
        assert_eq!(fmt_idle(1000), "1.0s");
        assert_eq!(fmt_idle(3450), "3.5s");
    }

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("500ms"), Some(500));
        assert_eq!(parse_duration("2s"), Some(2_000));
        assert_eq!(parse_duration("1m"), Some(60_000));
        assert_eq!(parse_duration("4h"), Some(14_400_000));
        assert_eq!(parse_duration("4hr"), Some(14_400_000));
        assert_eq!(parse_duration("1d"), Some(86_400_000));
        assert_eq!(parse_duration("30s"), Some(30_000));
    }

    #[test]
    fn parse_duration_rejects_malformed() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("s"), None); // no integer
        assert_eq!(parse_duration("10"), None); // no unit
        assert_eq!(parse_duration("10x"), None); // bad unit
        assert_eq!(parse_duration("abc"), None);
    }

    #[test]
    fn follow_events_are_one_json_object_per_line() {
        // Escapes survive: the payload is the raw pty stream, so ESC and CR
        // are normal content here, not formatting.
        assert_eq!(
            follow_event_json(&FollowEvent::Output("hi\x1b[0m\r\n"), 1_700_000_000_000),
            r#"{"event":"output","time_ms":1700000000000,"text":"hi\u001b[0m\r\n"}"#
        );
        assert_eq!(
            follow_event_json(
                &FollowEvent::Status {
                    running: false,
                    idle_ms: 2001
                },
                7
            ),
            r#"{"event":"status","time_ms":7,"running":false,"idle_ms":2001}"#
        );
        assert_eq!(
            follow_event_json(&FollowEvent::Exit, 7),
            r#"{"event":"exit","time_ms":7}"#
        );
        assert_eq!(
            follow_event_json(&FollowEvent::Timeout, 7),
            r#"{"event":"timeout","time_ms":7}"#
        );
    }

    #[test]
    fn utf8_stream_rejoins_characters_split_across_batches() {
        let mut s = Utf8Stream::default();
        // "中" is e4 b8 ad; the pty batch boundary falls inside it.
        assert_eq!(s.push(b"ab\xe4\xb8"), "ab");
        assert_eq!(s.push(b"\xad cd"), "中 cd");
        assert_eq!(s.flush(), "");
    }

    #[test]
    fn utf8_stream_replaces_truly_invalid_bytes_and_keeps_going() {
        let mut s = Utf8Stream::default();
        // A lone 0xff is not the start of anything: replace it, then keep the
        // trailing partial character back as usual.
        assert_eq!(s.push(b"a\xffb\xe4\xb8"), "a\u{fffd}b");
        assert_eq!(s.push(b"\xad"), "中");
        // A partial character at end of stream can never complete.
        assert_eq!(s.push(b"\xe4"), "");
        assert_eq!(s.flush(), "\u{fffd}");
        assert_eq!(s.flush(), "");
    }

    #[test]
    fn screen_model_reports_a_line_only_once_it_can_no_longer_change() {
        // Three rows of screen: the fourth line pushes the first one off.
        let mut m = ScreenModel::new(20, 3);
        // Still on the live screen, so still rewritable — nothing is final.
        assert_eq!(m.push(b"one\r\ntwo\r\n"), "");
        assert_eq!(m.screen(), "one\ntwo");
        // Scrolling is what makes a line final, in order and exactly once.
        assert_eq!(m.push(b"three\r\n"), "one");
        assert_eq!(m.push(b"four\r\nfive\r\n"), "two\nthree");
        assert_eq!(m.screen(), "four\nfive");
    }

    #[test]
    fn screen_model_ignores_a_status_line_repainted_in_place() {
        // What a TUI does ten times a second: rewrite one row via CR. Every
        // frame looks like new bytes; none of it is new output.
        let mut m = ScreenModel::new(40, 3);
        for frame in [
            "\r✻ 实现路由表…",
            "\r✽ 实现路由表… 2",
            "\r· 实现路由表… 3",
            "\r✶ 实现路由表… 4",
        ] {
            assert_eq!(m.push(frame.as_bytes()), "", "repaint reported as output");
        }
        // It is reported once, as the screen, with the newest content.
        assert_eq!(m.screen(), "✶ 实现路由表… 4");
    }

    #[test]
    fn screen_model_keeps_real_output_around_a_repaint() {
        let mut m = ScreenModel::new(40, 3);
        // A spinner repainting under a line of real output that then scrolls.
        assert_eq!(m.push(b"result: ok\r\n"), "");
        assert_eq!(m.push(b"\rworking 1"), "");
        assert_eq!(m.push(b"\rworking 2"), "");
        assert_eq!(m.push(b"\r\nsecond line\r\n"), "result: ok");
        assert_eq!(m.push(b"third line\r\n"), "working 2");
        assert_eq!(m.screen(), "second line\nthird line");
    }

    #[test]
    fn screen_model_trims_the_blank_rows_below_the_content() {
        // A screen is always `rows` tall; the padding is not content.
        let mut m = ScreenModel::new(20, 6);
        assert_eq!(m.push(b"a\r\nb\r\n"), "");
        assert_eq!(m.screen(), "a\nb");
    }

    #[test]
    fn enter_absorbs_a_line_ending_the_payload_already_had() {
        // What `echo x | send --stdin --enter` produces: the shell's newline
        // must not reach the pty as "insert a line break" before the Enter.
        assert_eq!(with_enter(b"make test\n".to_vec()), b"make test\r");
        assert_eq!(with_enter(b"make test\r\n".to_vec()), b"make test\r");
        // Nothing to absorb: just the Enter.
        assert_eq!(with_enter(b"make test".to_vec()), b"make test\r");
        assert_eq!(with_enter(Vec::new()), b"\r");
        // Only one line ending is folded in — a deliberate blank line stays.
        assert_eq!(with_enter(b"a\n\n".to_vec()), b"a\n\r");
        // An interior newline is content (a multi-line paste), not the ending.
        assert_eq!(with_enter(b"line1\nline2".to_vec()), b"line1\nline2\r");
    }

    #[test]
    fn scrollback_flag_maps_its_three_states() {
        // Absent, bare, and valued are three different requests.
        assert_eq!(scrollback_arg(None), Scrollback::None);
        assert_eq!(scrollback_arg(Some(None)), Scrollback::All);
        assert_eq!(scrollback_arg(Some(Some(200))), Scrollback::Lines(200));
        // `--scrollback 0` asks for no history, which is the screen alone.
        assert_eq!(scrollback_arg(Some(Some(0))), Scrollback::Lines(0));
    }

    #[test]
    fn json_string_escapes() {
        let mut s = String::new();
        json_string("a\"b\\c\nd\te", &mut s);
        assert_eq!(s, r#""a\"b\\c\nd\te""#);
        let mut s = String::new();
        json_string("\x01", &mut s);
        assert_eq!(s, "\"\\u0001\"");
    }
}
