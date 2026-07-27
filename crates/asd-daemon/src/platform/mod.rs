//! Everything the daemon does differently per platform, behind one surface.
//!
//! `unix.rs` and `win.rs` are both mounted here as `imp`, and the two
//! `cfg`s below are the only ones involved: the re-export is unconditional, so
//! a platform missing any item of the surface fails to compile with that item
//! named — the implementations cannot drift apart unnoticed. Callers say
//! `platform::serve_connections(..)` and never branch themselves.
//!
//! The surface:
//!
//! - [`serve_connections`] — bind the listener and run the accept loop until a
//!   termination signal, then shut the registry down.
//! - [`prepare_socket_dir`] / [`remove_stale_socket`] — make the listener's
//!   location usable before binding (no-ops where the OS has nothing to clean).
//! - [`kill_child`] — end a session's child process, gracefully or forcibly.
//! - [`read_cwd`] — the current working directory of a live process, for the
//!   persisted session list.
//! - [`pty_master_fd`] — the pty master's raw fd, for foreground-process
//!   lookups; `-1` where the platform has no fd to borrow.

#[cfg(unix)]
#[path = "unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "win.rs"]
mod imp;

pub(crate) use imp::{
    kill_child, prepare_socket_dir, pty_master_fd, read_cwd, remove_stale_socket, serve_connections,
};
