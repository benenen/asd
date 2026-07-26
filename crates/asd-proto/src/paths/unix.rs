//! Unix half of the path contract: a UDS under the runtime dir, XDG data and
//! config dirs. Selected by [`super`]; see there for the shared surface.

use std::path::{Path, PathBuf};

/// UDS file name.
const SOCKET_FILE: &str = "asd.sock";

/// Full path for the daemon listener: `$XDG_RUNTIME_DIR/asd.sock` (or the same
/// name under the fallback directory).
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
    runtime_dir().join(SOCKET_FILE)
}

/// PID file for the daemon owning `socket`: the socket path with a `.pid`
/// extension (`.../asd.sock` → `.../asd.pid`). The daemon writes its pid here
/// on startup and removes it on clean shutdown; `asd restart` reads it to stop
/// the running daemon by signal — no protocol handshake, so it works even when
/// the running daemon's `PROTO_VERSION` differs from the client's.
pub fn pid_path(socket: &Path) -> PathBuf {
    socket.with_extension("pid")
}

/// Daemon data directory: `$XDG_DATA_HOME/asd`, falling back to
/// `~/.local/share/asd`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("asd");
    }
    home_dir().join(".local/share/asd")
}

/// Directory holding the config file: `$XDG_CONFIG_HOME/asd`, falling back to
/// `~/.config/asd`.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("asd");
    }
    home_dir().join(".config/asd")
}

/// Directory holding the UDS: `$XDG_RUNTIME_DIR`, falling back to
/// `/tmp/asd-$UID` (which should be created 0700).
fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(format!("/tmp/asd-{}", uid())),
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Real uid of the current process (std has no API for this; obtained via
/// `/proc` metadata to avoid pulling in libc).
fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_path_swaps_the_socket_extension() {
        assert_eq!(
            pid_path(Path::new("/tmp/asd-0/asd.sock")),
            PathBuf::from("/tmp/asd-0/asd.pid")
        );
        assert_eq!(
            pid_path(Path::new("/run/user/1000/asd.sock")),
            PathBuf::from("/run/user/1000/asd.pid")
        );
        // A socket path with no extension just gains `.pid`.
        assert_eq!(
            pid_path(Path::new("/custom/mysock")),
            PathBuf::from("/custom/mysock.pid")
        );
    }
}
