//! Platform-specific host-terminal probing behind one shared surface.

#[cfg(unix)]
#[path = "unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "win.rs"]
mod imp;

pub(crate) use imp::probe_terminal_colors;
