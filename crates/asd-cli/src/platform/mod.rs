//! Everything the CLI does differently per platform, behind one surface.
//!
//! `unix.rs` and `win.rs` are both mounted here as `imp`, and the two `cfg`s
//! below are the only ones involved: the re-export is unconditional, so a
//! platform missing any item fails to compile with that item named — the two
//! implementations cannot drift apart unnoticed. Callers say
//! `platform::connect_stream(..)` and never branch themselves.
//!
//! The surface:
//!
//! - [`connect_stream`] — open the transport to the daemon's listener and split
//!   it into the two boxed halves the framed codec wants.
//! - [`configure_detached`] — make a spawned daemon outlive this process and
//!   own no terminal.
//! - [`stop_daemon`] — end the daemon recorded in the pid file, for a restart.
//! - [`RawGuard`] — put the terminal in raw mode, restore it on drop.
//! - [`install_terminating_signal_restore`] — hand the terminal back even when
//!   the process is killed, which skips every `Drop`.
//! - [`term_size`] — the terminal's cell dimensions.
//! - [`Winch`] / [`winch`] — a resize-notification source with a `recv()` that
//!   simply never fires where the platform has no such signal.
//! - [`run_stdio_proxy`] — the `--stdio` dumb-pipe proxy.

use std::path::Path;

use tokio::io::{AsyncRead, AsyncWrite};

#[cfg(unix)]
#[path = "unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "win.rs"]
mod imp;

pub(crate) use imp::{
    RawGuard, Winch, configure_detached, connect_stream, install_terminating_signal_restore,
    run_stdio_proxy, stop_daemon, term_size, winch,
};

/// The daemon side of a connection, type-erased for the framed codec.
pub(crate) type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub(crate) type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// The one "no daemon there" message, so both transports word it identically.
pub(crate) fn not_running(listener: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "asd-daemon is not running at {} (start one with `asd new` or `asd attach -A <name>`)",
        listener.display()
    )
}
