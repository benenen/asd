//! Windows half: the named-pipe transport plus terminal hygiene. Selected by
//! [`super`]; see there for the shared surface.
//!
//! The two hygiene entries are no-ops for now, and deliberately so rather than
//! by oversight:
//!
//! - Windows has no signal whose default action kills the process before
//!   cleanup runs. Console close and Ctrl+Break arrive as a `HandlerRoutine`
//!   callback on a separate thread with a hard timeout, which is a different
//!   mechanism than the unix handler and needs its own design.
//! - There is no ConPTY equivalent of the orphaned-pty EOF spin the unix
//!   watchdog exists to catch: a closed console signals the process rather than
//!   leaving a readable-forever handle.

use std::path::Path;

use super::{BoxRead, BoxWrite};

/// Connect to the daemon's named pipe and split it for the framed codec.
pub(crate) async fn connect_stream(socket: &Path) -> Result<(BoxRead, BoxWrite), String> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let name = socket
        .to_str()
        .ok_or_else(|| "pipe path is not valid UTF-8".to_string())?;
    let stream = ClientOptions::new()
        .open(name)
        .map_err(|e| format!("connect {name}: {e}"))?;
    let (r, w) = tokio::io::split(stream);
    Ok((Box::new(r), Box::new(w)))
}

/// No-op: see the module docs.
pub(crate) fn install_terminating_signal_restore() {}

/// No-op: see the module docs.
pub(crate) fn spawn_tty_watchdog() {}

/// No-op: reading another process's current directory on Windows means opening
/// the process and walking its PEB, which needs a privilege the TUI does not
/// ask for. The overlay reports that the directory could not be determined,
/// exactly as it does on a macOS host.
///
/// `#[allow(dead_code)]`: Task 11 wires the first call site into the overlay;
/// remove this once that caller lands.
#[allow(dead_code)]
pub(crate) fn session_cwd(_pid: u32) -> Option<std::path::PathBuf> {
    None
}
