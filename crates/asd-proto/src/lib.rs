//! asd wire protocol v0 (spec §4).
//!
//! Frame format: `u32 LE length prefix + postcard serialization`, 4 MiB cap
//! per frame; exceeding it is a protocol error → disconnect. Works over any
//! `AsyncRead + AsyncWrite` — the same codec serves the local UDS and the
//! remote SSH dumb pipe.
//!
//! Adding any frame requires bumping [`PROTO_VERSION`], with both ends
//! upgraded together; v0/v1 do not run multi-version compatible, a version
//! mismatch always gets `Error{code=1}` followed by disconnect.

mod codec;
pub mod paths;

pub use codec::{FrameReader, FrameWriter, decode_frame, encode_frame};

use serde::{Deserialize, Serialize};

/// Protocol version. Carried once in each direction via `Hello`/`HelloAck`;
/// any inequality is rejected.
pub const PROTO_VERSION: u32 = 0;

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
    /// Creation time, Unix epoch milliseconds.
    pub created_ms: u64,
    pub attached_clients: u32,
    pub cols: u16,
    pub rows: u16,
}

/// All frames of protocol v0 (spec §4).
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
