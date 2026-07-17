//! asd wire protocol v1 (spec §4).
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
//! (`SendInput`/`Ack`, `Peek`/`PeekReply`) and `SessionInfo.idle_ms`.

mod codec;
pub mod paths;

pub use codec::{FrameReader, FrameWriter, decode_frame, encode_frame};

use serde::{Deserialize, Serialize};

/// Protocol version. Carried once in each direction via `Hello`/`HelloAck`;
/// any inequality is rejected.
pub const PROTO_VERSION: u32 = 4;

/// Per-frame cap: 4 MiB (postcard payload, excluding the 4-byte length prefix).
pub const MAX_FRAME_LEN: usize = 4 * 1024 * 1024;

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
    /// Daemon internal error (details in msg).
    pub const INTERNAL: u32 = 100;
}

/// Client kind, self-reported during the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientKind {
    Gui,
    Cli,
    /// Remote proxy behind `asd attach --stdio` (used by M2/M3, reserved in protocol v0).
    Proxy,
}

/// Metadata for a single session in `SessionList`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub name: String,
    /// The command running in the session's terminal now — the pty's foreground
    /// process (e.g. `vim file`, `npm run dev`) — falling back to the spawn
    /// command (the `Create` cmd or the default shell) when it can't be
    /// resolved. Display-only.
    pub command: String,
    /// The terminal title as set by the session (OSC 0/2), e.g. a shell's
    /// `user@host: dir` or an app's own status line. Empty when never set.
    /// Display-only.
    pub title: String,
    /// Creation time, Unix epoch milliseconds.
    pub created_ms: u64,
    /// Milliseconds since the session last produced pty output; 0 while it is
    /// actively producing (or just created). Drives `asd wait --idle`.
    pub idle_ms: u64,
    pub attached_clients: u32,
    pub cols: u16,
    pub rows: u16,
}

/// All frames of protocol v1 (spec §4).
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
    },
    Created {
        name: String,
    },
    Kill {
        name: String,
    },
    // Attach and data plane
    Attach {
        name: String,
        cols: u16,
        rows: u16,
    },
    /// Formatter dump; the attach reply, also used for M3 flow-control recovery.
    Snapshot {
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
    // Scrollback (v1, spec §4). Rows are indexed in "screen space": row 0 is
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
    },
    /// daemon → client: generic success reply (answers `SendInput`).
    Ack,
    /// client → daemon: request a rendered plain-text dump of session `name`
    /// (`asd peek`). `scrollback` includes the full history above the screen.
    Peek {
        name: String,
        scrollback: bool,
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
    // Errors
    Error {
        code: u32,
        msg: String,
    },
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
