//! Windows half of the path contract: a named pipe for the listener, and the
//! two standard per-user application-data directories. Selected by [`super`];
//! see there for the shared surface.

use std::path::{Path, PathBuf};

/// Named-pipe path prefix.
const PIPE_PREFIX: &str = "\\\\.\\pipe\\";

/// Full path for the daemon listener: the named pipe `\\.\pipe\asd-<user>`.
///
/// The `ASD_SOCKET` environment variable overrides it entirely (tests and
/// multi-instance scenarios); the daemon and all clients honor the same
/// precedence.
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("ASD_SOCKET")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
    PathBuf::from(format!("{PIPE_PREFIX}asd-{user}"))
}

/// PID file for the daemon owning `socket`. `socket` here is a named pipe,
/// which lives in the kernel's pipe namespace and cannot hold a file — appending
/// `.pid` to it would just name a second pipe. The pid file therefore goes in
/// [`data_dir`], named after the pipe so a custom `ASD_SOCKET` still gets its
/// own file: `<data_dir>/asd-<user>.pid`.
pub fn pid_path(socket: &Path) -> PathBuf {
    let base = socket
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "asd".to_string());
    data_dir().join(format!("{base}.pid"))
}

/// Daemon data directory: `%LOCALAPPDATA%\asd`, falling back to
/// `%USERPROFILE%\AppData\Local\asd`. Machine-local read-write state, kept
/// deliberately apart from [`config_dir`].
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LOCALAPPDATA")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("asd");
    }
    home_dir().join("AppData").join("Local").join("asd")
}

/// Directory holding the (read-only, user-authored) config file: roaming
/// application data, `%APPDATA%\asd` — deliberately not [`data_dir`].
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("APPDATA")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("asd");
    }
    home_dir().join("AppData").join("Roaming").join("asd")
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\"))
}
