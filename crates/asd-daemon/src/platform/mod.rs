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
//! - [`harden_dll_search`] — constrain where the process may load libraries
//!   from, before anything gets the chance to (a no-op where the loader has no
//!   such search path).
//! - [`kill_child`] — end a session's child process, gracefully or forcibly.
//! - [`watch_child_exit`] — report a child's exit to its session thread, where
//!   the pty does not go to EOF with it (a no-op where it does).
//! - [`read_cwd`] — the current working directory of a live process, for the
//!   persisted session list.
//! - [`pty_master_fd`] — the pty master's raw fd, for foreground-process
//!   lookups; `-1` where the platform has no fd to borrow.
//! - [`foreground_agent`] — actual executable/argv proof for lifecycle hooks,
//!   separate from display-oriented shell command formatting.
//! - [`create_private_temp`] / [`replace_file`] / [`sync_parent`] — the
//!   persistence store's private atomic-write primitive.

#[cfg(unix)]
mod foreground;
#[cfg(unix)]
#[path = "unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "win.rs"]
mod imp;

pub(crate) use imp::{
    create_private_temp, foreground_agent, harden_dll_search, kill_child, prepare_socket_dir,
    pty_master_fd, read_cwd, remove_stale_socket, replace_file, serve_connections, set_session_env,
    sync_parent, watch_child_exit,
};
