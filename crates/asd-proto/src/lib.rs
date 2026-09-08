//! asd wire protocol.
//!
//! Frame format: `u32 LE length prefix + postcard serialization`, 4 MiB cap
//! per frame; exceeding it is a protocol error → disconnect. Works over any
//! `AsyncRead + AsyncWrite` — the same codec serves the local UDS and the
//! remote SSH dumb pipe.
//!
//! Adding any frame — or changing a frame's shape — requires bumping
//! [`PROTO_VERSION`], with both ends upgraded together; the protocol does not
//! run multi-version compatible, a version mismatch always gets `Error{code=1}`
//! followed by disconnect. v1 added the scrollback frames
//! (`FetchHistory`/`History`) and `Refresh`; v2 added `SessionInfo.command`;
//! v3 added `SessionInfo.title`; v4 added the attach-free scripting frames
//! (`SendInput`/`Ack`, `Peek`/`PeekReply`) and `SessionInfo.idle_ms`; v5 added
//! `SessionInfo.running` (idle-derived activity flag); v6 added the
//! `Inspect`/`InspectReply` frames (detailed single-session dump); v7 added
//! `Rename` (rename a session; replies `Ack`); v8 added `Create.cwd` (start the
//! session in a given directory, instead of the caller folding a `cd` into
//! `cmd` — which also made the persisted cwd wrong at create time) and
//! `SessionInfo.pid` (so `list` answers what previously took an `inspect` per
//! session); v9 added the following frames (`Follow`/`Unfollow`/`FollowStatus`)
//! behind `asd follow` — an output subscription that reports quiescence inline
//! rather than making the client poll for it; v10 replaced `Peek.scrollback`'s
//! boolean with [`Scrollback`]; v11 added `Attach.appearance`, letting a real
//! terminal client report its default colors so the daemon can answer OSC
//! 10/11 queries without guessing a theme; v12 added `SendInput.enter`, making
//! a scripted payload plus Enter one atomic session-thread operation; v13
//! identifies TUI clients and adds `ViewRevoked`/`ViewRenamed`, allowing one
//! interactive TUI viewer per session while ordinary attach clients remain
//! shared and external renames keep the owner tagged correctly; v14 adds
//! `SessionInfo.state` and `FollowStatus.state`, the daemon's reading of what
//! the program on the screen is doing — distinct from `running`, which only
//! says whether bytes are arriving; v15 adds `HostMetrics`/`HostMetricsReply`,
//! letting a client read the daemon host's CPU, memory and network rates. The
//! daemon samples them on its own timer and answers from that reading, so the
//! request never measures anything and no client can drive the sampling rate;
//! v16 adds `Attach.read_only`, a viewer that receives the Snapshot and every
//! Output while the daemon drops its input and keeps it out of the pty's size
//! negotiation; v17 adds `FollowStatus.exit`, the exit code or signal that
//! ended a session's child — reported on the last status a session sends,
//! because by then the session has left the registry and no `SessionList` can
//! carry it; v18 adds `SetStatusLine` and `SessionInfo.status_line`, one line a
//! session sets about itself from the inside — the only progress channel that
//! does not go through reading the screen; v19 adds the opaque
//! `SessionInfo.instance_id` and carries it as [`SessionIdentity`] in `Kill`,
//! so a delayed confirmation cannot terminate a newer session that reused the
//! same name; v20 adds agent-operation frames and includes the exact live
//! [`SessionIdentity`] in each `Snapshot`, so a client can converge its view
//! and later output on the same session instance; v21 adds durable task
//! associations and daemon-side worktree review frames; v22 adds bounded
//! daemon-side workspace directory listings.

mod codec;
pub mod paths;

pub use codec::{FrameReader, FrameWriter, decode_frame, encode_frame};

use serde::{Deserialize, Serialize};

/// Protocol version. Carried once in each direction via `Hello`/`HelloAck`;
/// any inequality is rejected.
pub const PROTO_VERSION: u32 = 22;

/// Output-quiescence threshold, in milliseconds. A session is considered
/// **idle** once its pty has produced no output for this long, and **running**
/// otherwise. Shared so `SessionInfo.running` (daemon) and `asd wait --idle`
/// (client) agree on one definition.
pub const IDLE_SETTLE_MS: u64 = 2000;

/// What the program in a session is doing, as the daemon reads it off the
/// rendered screen.
///
/// Deliberately not the same question as [`SessionInfo::running`], which is
/// byte activity and cannot tell a busy-but-silent program from one that has
/// stopped to ask the user something. Only the daemon may set this: it owns
/// the session-side terminal model, so every client sees one answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    /// Busy on a turn of its own.
    Working,
    /// Stopped, waiting for a person: a permission prompt, a question, a
    /// selection.
    Blocked,
    /// Ready for input, with nothing pending.
    Idle,
    /// Nothing recognized, or a screen the daemon declines to classify. Never
    /// a claim that something *is* finished.
    #[default]
    Unknown,
}

impl AgentState {
    /// The lowercase name, for display and for `--until` arguments.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Idle => "idle",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AgentState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "working" => Ok(Self::Working),
            "blocked" => Ok(Self::Blocked),
            "idle" => Ok(Self::Idle),
            "unknown" => Ok(Self::Unknown),
            other => Err(format!(
                "unknown state {other:?} (want working, blocked, idle, or unknown)"
            )),
        }
    }
}

/// Per-frame cap: 4 MiB (postcard payload, excluding the 4-byte length prefix).
pub const MAX_FRAME_LEN: usize = 4 * 1024 * 1024;

/// Maximum bytes in a literal or regular-expression screen matcher.
pub const MAX_SCREEN_PATTERN: usize = 4096;
/// Maximum concurrent screen waiters in one session.
pub const MAX_SCREEN_WAITERS: usize = 64;
/// Number of committed session events retained for replay.
pub const EVENT_RING_CAPACITY: usize = 512;

/// Error codes for the `Error` frame.
pub mod code {
    /// `proto_version` mismatch; daemon sends this error then disconnects.
    pub const VERSION_MISMATCH: u32 = 1;
    /// Target session does not exist.
    pub const NO_SUCH_SESSION: u32 = 2;
    /// The session named in create already exists.
    pub const SESSION_EXISTS: u32 = 3;
    /// Session name does not satisfy `[A-Za-z0-9_-]{1,64}`.
    pub const INVALID_NAME: u32 = 4;
    /// A connection may attach to at most one session at a time.
    pub const ALREADY_ATTACHED: u32 = 5;
    /// The session's child process has exited; the session is destroyed with it.
    pub const SESSION_EXITED: u32 = 6;
    /// Business frame sent before completing the handshake, or invalid frame order.
    pub const BAD_HANDSHAKE: u32 = 7;
    /// The named session exists, but it is not the instance the caller observed.
    pub const STALE_SESSION: u32 = 8;
    /// A screen matcher is malformed or exceeds the protocol limit.
    pub const INVALID_MATCHER: u32 = 9;
    /// A session already has the maximum number of screen waiters.
    pub const SCREEN_WAITER_LIMIT: u32 = 10;
    /// A screen waiter reached its requested deadline.
    pub const WAIT_TIMEOUT: u32 = 11;
    /// An agent hook report is malformed or inconsistent with the session.
    pub const INVALID_AGENT_REPORT: u32 = 12;
    /// Session-state persistence failed.
    pub const PERSISTENCE_FAILURE: u32 = 13;
    /// Daemon internal error (details in msg).
    pub const INVALID_TASK: u32 = 14;
    pub const REVIEW_FAILED: u32 = 15;
    pub const INTERNAL: u32 = 100;
}

/// Client kind, self-reported during the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientKind {
    Gui,
    Cli,
    /// Remote proxy behind `asd attach --stdio`.
    Proxy,
    /// Ratatui `asd ui`; only one TUI may hold a session's interactive view.
    Tui,
}

/// How a session's child ended, as the platform reported it.
///
/// Reported once, on the last `FollowStatus` a session ever sends. It is not on
/// [`SessionInfo`], and cannot be: by the time the status is known the session
/// is out of the registry, so no `list` will ever carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionExit {
    /// Exit code as the platform reported it. A child ended by a signal reports
    /// `1` here and names the signal below.
    pub code: u32,
    /// The signal that ended the child, when one did. This is the platform's
    /// own description rather than the `SIG*` constant — `Hangup`, `Killed` —
    /// because that is what the pty layer hands over, and inventing a mapping
    /// back would mean guessing at a number nobody passed us. Always `None` on
    /// Windows, which has no signals.
    pub signal: Option<String>,
}

impl SessionExit {
    /// Whether the child ended of its own accord, successfully.
    pub fn success(&self) -> bool {
        self.signal.is_none() && self.code == 0
    }
}

impl std::fmt::Display for SessionExit {
    /// Short enough to read inside another sentence: `status 0`, `status 3`,
    /// `signal SIGKILL`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.signal {
            Some(signal) => write!(f, "signal {signal}"),
            None => write!(f, "status {}", self.code),
        }
    }
}

/// Opaque identity of one live session instance.
///
/// The daemon generates a fresh value when it creates the session and treats
/// a client-provided identity only as an assertion against its own Registry
/// entry. It is never used as a process identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionIdentity {
    pub instance_id: u128,
}

impl std::fmt::Display for SessionIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.instance_id)
    }
}

impl std::str::FromStr for SessionIdentity {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("session identity must be exactly 32 hexadecimal characters".to_string());
        }
        u128::from_str_radix(value, 16)
            .map(|instance_id| Self { instance_id })
            .map_err(|error| format!("invalid session identity: {error}"))
    }
}

/// Cursor into the daemon-local session-event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCursor {
    pub daemon_epoch: [u8; 16],
    pub sequence: u64,
}

/// Why a session projection changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionUpdateCause {
    Registered,
    ActivityStarted,
    ActivitySettled,
    ForegroundChanged,
    AttachmentChanged,
    StatusLineChanged,
    TaskChanged,
    ScreenDetection,
    DetectorReload,
}

/// A name-free subset of mutable session metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUpdatePatch {
    pub command: Option<String>,
    pub title: Option<String>,
    pub status_line: Option<String>,
    /// None leaves the task unchanged; Some(None) clears it.
    pub task: Option<Option<SessionTask>>,
    pub idle_ms: Option<u64>,
    pub running: Option<bool>,
    pub state: Option<AgentState>,
    pub attached_clients: Option<u32>,
    pub pid: Option<u32>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
}

/// A committed change in the daemon's session projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEvent {
    Registered {
        info: SessionInfo,
    },
    Updated {
        identity: SessionIdentity,
        patch: SessionUpdatePatch,
        cause: SessionUpdateCause,
    },
    Renamed {
        old_name: String,
        info: SessionInfo,
    },
    Exited {
        identity: SessionIdentity,
        last_name: String,
        exit: SessionExit,
    },
}

/// A string or regular-expression condition evaluated against a rendered screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScreenMatcher {
    Literal(String),
    Regex(String),
}

/// Supported agent hook producers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentKind {
    Codex,
    Claude,
}

impl AgentKind {
    /// Lowercase name used by manifests and hook environment values.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AgentKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            other => Err(format!(
                "unknown agent kind {other:?} (want codex or claude)"
            )),
        }
    }
}

/// Lifecycle update supplied by an agent hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentHookAction {
    Start { source: String },
    End { reason: String },
}

/// One detection rule's outcome in an explanation response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionRuleReport {
    pub manifest_id: String,
    pub rule_id: String,
    pub priority: i32,
    pub state: AgentState,
    pub region: String,
    pub matched: bool,
    pub evidence: Vec<String>,
    pub reason: Option<String>,
}

/// The daemon's detection reasoning for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionReport {
    pub foreground_command: String,
    pub candidate_manifest_ids: Vec<String>,
    pub selected_manifest_id: Option<String>,
    pub state: AgentState,
    pub rules: Vec<DetectionRuleReport>,
    pub generation: u64,
}

/// One diagnostic emitted while reloading agent manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestDiagnostic {
    pub path: String,
    pub manifest_id: Option<String>,
    pub message: String,
    pub retained_previous: bool,
}

/// A durable task association. The directory belongs to the daemon host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTask {
    pub description: String,
    pub directory: String,
}

impl SessionTask {
    /// Validate portable text constraints; the daemon validates the filesystem.
    pub fn validate(&self) -> Result<(), String> {
        if self.description.trim().is_empty() || self.description.len() > 4096 {
            return Err("task description must contain 1 to 4096 bytes".into());
        }
        if self
            .description
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return Err("task description contains unsupported control characters".into());
        }
        if self.directory.is_empty()
            || self.directory.len() > 4096
            || self.directory.chars().any(char::is_control)
        {
            return Err(
                "task directory must contain 1 to 4096 bytes without control characters".into(),
            );
        }
        Ok(())
    }
}

/// Metadata for a single session in `SessionList`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub name: String,
    /// Opaque daemon-generated identity for this live session instance.
    pub instance_id: u128,
    /// The command running in the session's terminal now — the pty's foreground
    /// process (e.g. `vim file`, `npm run dev`) — falling back to the spawn
    /// command (the `Create` cmd or the default shell) when it can't be
    /// resolved. Display-only.
    pub command: String,
    /// The terminal title as set by the session (OSC 0/2), e.g. a shell's
    /// `user@host: dir` or an app's own status line. Empty when never set.
    /// Display-only.
    pub title: String,
    /// What the session says it is doing, set from inside it with `asd status`.
    /// Empty when nothing set it.
    ///
    /// Distinct from `title` and from `state`: the title is whatever the
    /// program happened to put in the terminal's title bar, and the state is
    /// the daemon's reading of the screen. This is the one channel where the
    /// program speaks for itself, so it can say "step 3/7" — something no
    /// amount of screen-scraping can work out.
    pub status_line: String,
    pub task: Option<SessionTask>,
    /// Creation time, Unix epoch milliseconds.
    pub created_ms: u64,
    /// Milliseconds since the session last produced pty output; 0 while it is
    /// actively producing (or just created). Drives `asd wait --idle`.
    pub idle_ms: u64,
    /// Whether the session is actively producing output — `idle_ms <
    /// IDLE_SETTLE_MS`. For an agent (claude/codex/opencode) this reads as
    /// "working" vs "done / waiting for input"; the title only labels *what*
    /// it is, it does not flip with activity. Daemon-derived.
    pub running: bool,
    /// What the program on the screen is doing, where the daemon recognizes it
    /// — an agent working, blocked on a prompt, or ready. [`AgentState::Unknown`]
    /// for everything else, including every ordinary shell. Screen-derived, so
    /// it answers a different question than `running` above: an agent stopped
    /// at a permission prompt is `Blocked` and not running, while one thinking
    /// silently is `Working` and also not running.
    pub state: AgentState,
    pub attached_clients: u32,
    /// The session child's process id, or 0 before it is known. Lets a caller
    /// reach the process (its `/proc` entry, say) straight from `list`, instead
    /// of an `inspect` round trip per session.
    pub pid: u32,
    pub cols: u16,
    pub rows: u16,
}

impl SessionInfo {
    pub fn identity(&self) -> SessionIdentity {
        SessionIdentity {
            instance_id: self.instance_id,
        }
    }
}

/// How much of a session's history a [`Frame::Peek`] should include above the
/// live screen.
///
/// The limit is applied by the daemon, not the caller: a session can retain
/// tens of thousands of lines, and the whole dump has to fit in one frame
/// ([`MAX_FRAME_LEN`]), so "the last N lines" has to mean "send N", not "send
/// everything and let the client cut".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scrollback {
    /// The live screen only.
    None,
    /// Every line the session still retains.
    All,
    /// At most this many lines above the screen (0 is the same as `None`).
    Lines(u32),
}

/// One RGB color reported by a terminal client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// The real host terminal's default colors, when it answered the client's
/// startup OSC 10/11 probes. Either channel may be unknown on terminals that
/// do not implement the corresponding query.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalAppearance {
    pub foreground: Option<TerminalColor>,
    pub background: Option<TerminalColor>,
}

/// One reading of the daemon host's resource use, taken by the daemon's own
/// sampler rather than measured when a client asks. Rates are per second.
///
/// Every field is an integer so that `Frame` keeps its `Eq`. CPU is the one
/// value the host reports as a float, and the bar renders it as a whole
/// percent anyway, so it is rounded at the sampler rather than carried at a
/// precision nothing consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSample {
    /// Whole-host utilisation, 0-100, averaged across cores.
    pub cpu_pct: u8,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    /// Bytes per second, summed over every non-loopback interface.
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
    /// How old this reading is, in milliseconds. Deliberately an age and not a
    /// timestamp: two hosts need not agree on the wall clock, so an absolute
    /// time could not be compared against the client's.
    pub sampled_age_ms: u64,
}

/// All frames in the current protocol.
///
/// Handshake: each side sends once after connecting; the client sends
/// `Hello` first.
/// Attach sequence: `Attach` → daemon replies `Snapshot` → subsequent
/// `Output` stream; the client must finish feeding the Snapshot before
/// consuming Output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Frame {
    // Handshake
    Hello {
        proto_version: u32,
        kind: ClientKind,
    },
    HelloAck {
        proto_version: u32,
        daemon_version: String,
    },
    // Session management
    ListSessions,
    SessionList {
        sessions: Vec<SessionInfo>,
    },
    /// `cmd` defaults to `$SHELL`. When `name` is omitted the daemon
    /// auto-assigns `s0`, `s1`, ...
    Create {
        name: Option<String>,
        cmd: Option<String>,
        /// Directory to start the session in; `None` inherits the daemon's.
        /// A path the daemon cannot enter fails the create rather than falling
        /// back, so a caller is never silently given the wrong directory.
        cwd: Option<String>,
    },
    Created {
        name: String,
    },
    /// client → daemon: terminate a session. There is no success reply. CLI
    /// callers confirm ordered handling by immediately sending `ListSessions`
    /// and waiting for its `SessionList`; failures still use `Error`.
    Kill {
        name: String,
        /// The exact live instance the caller confirmed. The daemon compares
        /// this with its Registry entry before signaling the child.
        identity: SessionIdentity,
    },
    /// client → daemon: rename session `name` to `new_name` (v7). Replies `Ack`
    /// on success, or `Error` (invalid/duplicate name, or no such session).
    Rename {
        name: String,
        new_name: String,
    },
    // Attach and data plane
    Attach {
        name: String,
        cols: u16,
        rows: u16,
        /// Client-generated identity for this view attempt. TUI clients use a
        /// fresh nonzero value so a delayed revoke cannot target a later view;
        /// shared clients send zero.
        view_id: u64,
        /// Defaults probed from the client's real terminal. The session locks
        /// each first non-`None` channel so multiple viewers cannot flip an
        /// application's theme underneath it.
        appearance: TerminalAppearance,
        /// Watch without touching: the daemon drops this client's `Input` and
        /// `Resize`, and leaves it out of size negotiation entirely, so a
        /// viewer can neither type into the session nor shrink it. It still
        /// receives the Snapshot and every Output.
        ///
        /// A guard against accidents, not an access boundary — the same
        /// connection can still drive the session with the attach-free
        /// scripting frames, as `tmux attach -r` can still `send-keys`.
        read_only: bool,
    },
    /// Formatter dump used for attach and flow-control recovery.
    Snapshot {
        /// Exact session instance that produced this terminal dump.
        identity: SessionIdentity,
        vt: Vec<u8>,
    },
    /// daemon → client, raw pty output.
    Output {
        bytes: Vec<u8>,
    },
    /// client → daemon, encoded keystrokes/paste.
    Input {
        bytes: Vec<u8>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Detach,
    // Scrollback. Rows are indexed in "screen space": row 0 is
    // the oldest scrollback line, row `total_rows - 1` is the bottom of the
    // live screen. The live view is the bottom `rows` of this space.
    /// client → daemon: request the row window `[start, start + count)`.
    FetchHistory {
        start: u32,
        count: u32,
    },
    /// daemon → client: the requested window. `rows` are plain UTF-8 text
    /// lines (one screen row each), trailing blanks trimmed; `total_rows`
    /// and `start` let the client clamp and render a scroll position.
    History {
        total_rows: u32,
        start: u32,
        rows: Vec<Vec<u8>>,
    },
    /// client → daemon: request a fresh `Snapshot` of the live screen (used
    /// to resync after leaving the client's local scrollback view).
    Refresh,
    // Scripting (v4). Name-addressed and attach-free: `send`/`peek` act on a
    // session by name without joining its broadcast list, so they work while
    // others are attached or with nobody attached.
    /// client → daemon: write raw bytes to session `name`'s pty (`asd send`).
    SendInput {
        name: String,
        bytes: Vec<u8>,
        /// After writing `bytes`, pause outside the target TUI's paste burst and
        /// write one carriage return before acknowledging the request.
        enter: bool,
    },
    /// daemon → client: generic success reply (answers `SendInput`).
    Ack,
    /// client → daemon: request a rendered plain-text dump of session `name`
    /// (`asd peek`). `scrollback` says how much history to put above the
    /// screen.
    Peek {
        name: String,
        scrollback: Scrollback,
    },
    /// daemon → client: the rendered screen plus geometry. `screen` is plain
    /// UTF-8 (one screen row per line, trailing blank lines trimmed); cursor
    /// coordinates are 0-based viewport cells.
    PeekReply {
        cols: u16,
        rows: u16,
        cursor_col: u16,
        cursor_row: u16,
        title: String,
        screen: Vec<u8>,
    },
    /// client → daemon: request a detailed dump of session `name` (`asd
    /// inspect`), gathered on the session thread (metadata + live VT state).
    Inspect {
        name: String,
    },
    /// daemon → client: everything known about one session — its `SessionInfo`
    /// metadata plus live internals (child pid, alternate-screen, scrollback
    /// depth, mouse tracking, cursor).
    InspectReply {
        info: SessionInfo,
        /// The session child's process id (0 once it has exited).
        child_pid: u32,
        /// Whether the terminal is on its alternate screen (a full-screen TUI).
        alt_screen: bool,
        /// Scrollback lines held above the live screen.
        scrollback_rows: u32,
        /// Whether the program is requesting mouse events.
        mouse_tracking: bool,
        /// The active DEC mouse modes (e.g. 1002, 1006), ascending.
        mouse_modes: Vec<u16>,
        /// Cursor viewport position (0-based) and visibility.
        cursor_col: u16,
        cursor_row: u16,
        cursor_visible: bool,
    },
    /// client → daemon: what is the daemon host's resource use right now.
    HostMetrics,
    /// daemon → client: the sampler's most recent reading. `None` means the
    /// sampler has not produced one yet, which is true for the first second of
    /// a daemon's life. It is `None` rather than zeroes because zeroes would
    /// claim the host is idle.
    HostMetricsReply {
        sample: Option<HostSample>,
    },
    // Following (v9). Like `send`/`peek`, name-addressed and attach-free — but
    // a subscription rather than a one-shot: the follower joins the Output
    // broadcast without taking part in size negotiation and without asking for
    // a Snapshot, so watching a session never changes what anyone else sees.
    /// client → daemon: stream session `name`'s output on this connection.
    /// Answered immediately with one `FollowStatus` describing the session as
    /// it stands, then an `Output` + `FollowStatus` pair per pty batch.
    Follow {
        name: String,
    },
    /// client → daemon: stop streaming. Dropping the connection does the same
    /// thing; this is for a client that wants to keep talking afterwards.
    Unfollow {
        name: String,
    },
    /// daemon → client: whether the session is still producing output, and how
    /// long it has been quiet. `running` is `idle_ms < IDLE_SETTLE_MS` — the
    /// definition `SessionInfo.running` and `asd wait --idle` already use, so
    /// three ways of asking "is it done?" cannot drift apart.
    FollowStatus {
        running: bool,
        /// What the program on the screen is doing. Carried here as well as in
        /// `SessionInfo` so a follower watching an agent work sees it stop —
        /// and sees *why* it stopped — without polling `list` alongside the
        /// stream it is already reading.
        state: AgentState,
        idle_ms: u64,
        /// Set only on the status that reports the session's end, where it says
        /// how the child ended. `running: false` alone cannot: it is also how a
        /// quiet spell is reported mid-stream.
        exit: Option<SessionExit>,
    },
    /// daemon → TUI: another `asd ui` took this session's exclusive view.
    /// The connection stays open so the displaced UI can keep listing sessions
    /// and explicitly select this one again to take it back.
    ViewRevoked {
        name: String,
        view_id: u64,
    },
    /// daemon → owning TUI: the viewed session was renamed by any client.
    /// This keeps frame routing and sidebar identity aligned even when a list
    /// response and the session-thread notification race across daemon tasks.
    ViewRenamed {
        old_name: String,
        new_name: String,
        view_id: u64,
    },
    // Errors
    Error {
        code: u32,
        msg: String,
    },
    /// client → daemon: set (or clear, with an empty `line`) what a session
    /// says it is doing. Name-addressed and attach-free, like the other
    /// scripting frames, because the caller is usually the program *inside*
    /// that session using `$ASD_SESSION`. Replies `Ack`, or `Error` when there
    /// is no such session.
    ///
    /// Last on purpose. postcard numbers variants by position, so a frame added
    /// anywhere else renumbers everything after it — and the first casualty is
    /// `Error` itself, which is how a daemon tells a client their versions
    /// disagree. Appending keeps that message readable to a newer client, so an
    /// upgrade in the wrong order says "proto version mismatch" instead of a
    /// codec error. New frames go here.
    SetStatusLine {
        name: String,
        line: String,
    },
    SubscribeEvents {
        after: Option<EventCursor>,
        wants_notifications: bool,
    },
    EventStreamStarted {
        cursor: EventCursor,
        sessions: Vec<SessionInfo>,
        reset: bool,
        notification_lease: bool,
    },
    SessionEvent {
        cursor: EventCursor,
        event: SessionEvent,
    },
    NotificationLeaseChanged {
        granted: bool,
    },
    WaitForScreen {
        name: String,
        matcher: ScreenMatcher,
        timeout_ms: u64,
    },
    ScreenWaitMatched {
        identity: SessionIdentity,
    },
    AgentExplain {
        name: String,
    },
    AgentExplainReply {
        report: DetectionReport,
    },
    ReloadAgentManifests,
    AgentManifestsReloaded {
        generation: u64,
        diagnostics: Vec<ManifestDiagnostic>,
        pending_identities: Vec<SessionIdentity>,
    },
    ReportAgentSession {
        identity: SessionIdentity,
        kind: AgentKind,
        action: AgentHookAction,
        session_ref: String,
    },
    AgentSessionReported,
    ClearAgentSession {
        identity: SessionIdentity,
    },
    AgentSessionCleared,
    /// Persist the task association for this exact live session.
    SetSessionTask {
        identity: SessionIdentity,
        task: Option<SessionTask>,
    },
    /// List a relative directory inside the associated workspace or live cwd.
    ListWorkspaceFiles {
        identity: SessionIdentity,
        path: String,
    },
    WorkspaceFiles {
        identity: SessionIdentity,
        root: String,
        path: String,
        entries: Vec<WorkspaceEntry>,
        truncated: bool,
    },
    /// Read changes in the associated worktree, or the daemon-side live cwd.
    GetSessionReview {
        identity: SessionIdentity,
    },
    SessionReview {
        identity: SessionIdentity,
        task: Option<SessionTask>,
        directory: String,
        branch: String,
        status: String,
        diff: String,
        truncated: bool,
    },
}

/// A workspace entry name is an exact UTF-8 path component, not display text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    pub name: String,
    pub kind: WorkspaceEntryKind,
}

/// Symlinks are listed but cannot be navigated through workspace requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceEntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

/// Protocol-layer error.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    /// Frame length exceeds [`MAX_FRAME_LEN`]; per the contract this is a
    /// protocol error and the caller should disconnect.
    #[error("frame length {0} exceeds {MAX_FRAME_LEN} byte cap")]
    FrameTooLarge(usize),
    #[error("postcard codec error: {0}")]
    Codec(#[from] postcard::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
