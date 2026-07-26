//! Windows terminal hygiene. Selected by [`super`]; see there for the shared
//! surface.
//!
//! Both entries are no-ops for now, and deliberately so rather than by
//! oversight:
//!
//! - Windows has no signal whose default action kills the process before
//!   cleanup runs. Console close and Ctrl+Break arrive as a `HandlerRoutine`
//!   callback on a separate thread with a hard timeout, which is a different
//!   mechanism than the unix handler and needs its own design.
//! - There is no ConPTY equivalent of the orphaned-pty EOF spin the unix
//!   watchdog exists to catch: a closed console signals the process rather than
//!   leaving a readable-forever handle.

/// No-op: see the module docs.
pub(crate) fn install_terminating_signal_restore() {}

/// No-op: see the module docs.
pub(crate) fn spawn_tty_watchdog() {}
