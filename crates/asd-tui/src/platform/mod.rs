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
//! - [`session_cwd`] — the working directory of a live local process, for
//!   resolving which repository a session is sitting in. `None` on platforms
//!   without a way to read it.
//!
//! [`install_terminating_signal_restore`] and [`spawn_tty_watchdog`] are
//! best-effort terminal hygiene, and [`session_cwd`] is best-effort in the same
//! way: a platform without the mechanism provides a no-op — or `None` — rather
//! than making callers ask whether it exists.

use tokio::io::{AsyncRead, AsyncWrite};

#[cfg(unix)]
#[path = "unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "win.rs"]
mod imp;

pub(crate) use imp::{
    connect_stream, install_terminating_signal_restore, session_cwd, spawn_tty_watchdog,
};

/// The daemon side of a connection, type-erased for the framed codec.
pub(crate) type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub(crate) type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;
