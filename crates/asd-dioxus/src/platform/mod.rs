//! The one thing the GUI does differently per platform, behind one surface.
//!
//! `unix.rs` and `win.rs` are both mounted here as `imp`, and the two `cfg`s
//! below are the only ones involved: the re-export is unconditional, so a
//! platform missing the item fails to compile with it named.
//!
//! The surface is [`connect_local`] — open a transport to the local daemon.

#[cfg(unix)]
#[path = "unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "win.rs"]
mod imp;

pub(crate) use imp::connect_local;
