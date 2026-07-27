//! Everything the TUI does differently per platform, behind one surface.
//!
//! `unix.rs` and `win.rs` are both mounted here as `imp`, and the two `cfg`s
//! below are the only ones involved: the re-export is unconditional, so a
//! platform missing an item fails to compile with that item named.
//!
//! The surface:
//!
//! - [`connect_stream`] — open the transport to the daemon's listener and split
//!   it into the two boxed halves the framed codec wants.
//! - [`install_terminating_signal_restore`] — put the terminal back when the
//!   process is killed outright, which normal cleanup never sees.
//! - [`spawn_tty_watchdog`] — exit when the hosting terminal disappears.
//!
//! The latter two are best-effort terminal hygiene: a platform without the
//! mechanism provides a no-op rather than making callers ask whether it exists.

use tokio::io::{AsyncRead, AsyncWrite};

#[cfg(unix)]
#[path = "unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "win.rs"]
mod imp;

pub(crate) use imp::{connect_stream, install_terminating_signal_restore, spawn_tty_watchdog};

/// The daemon side of a connection, type-erased for the framed codec.
pub(crate) type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub(crate) type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// Escape sequences that undo the modes the TUI turns on: disable mouse tracking
/// (SGR 1006/1015 + 1000/1002/1003), bracketed paste (2004), leave the alternate
/// screen (1049), show the cursor (25h), reset SGR (0m).
pub(crate) const TERM_RESTORE: &[u8] =
    b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?1049l\x1b[?25h\x1b[0m";
