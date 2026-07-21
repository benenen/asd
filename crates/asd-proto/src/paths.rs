//! Naming and path contract (spec §2).
//!
//! The daemon and all clients share this single convention; this module does
//! pure path computation only — directory creation (including 0700
//! permissions) is the responsibility of the daemon/spawner.

use std::path::{Path, PathBuf};

/// Maximum session name length.
pub const SESSION_NAME_MAX: usize = 64;

/// UDS file name.
pub const SOCKET_FILE: &str = "asd.sock";

/// Session name contract: `[A-Za-z0-9_-]{1,64}`.
pub fn is_valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= SESSION_NAME_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Directory holding the UDS: `$XDG_RUNTIME_DIR`, falling back to
/// `/tmp/asd-$UID` (which should be created 0700).
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(format!("/tmp/asd-{}", uid())),
    }
}

/// Full UDS path: `$XDG_RUNTIME_DIR/asd.sock` (or the same name under the
/// fallback directory).
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

/// Where the daemon records each session's workspace (cwd) when asked to restart
/// (SIGUSR1); the successor daemon consumes it to recreate the sessions in their
/// old directories. Per-socket, a sibling of the pid file.
pub fn restart_state_path(socket: &Path) -> PathBuf {
    socket.with_extension("restart")
}

/// Daemon data directory: `~/.local/share/asd/` (session metadata, logs).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("asd");
    }
    home_dir().join(".local/share/asd")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Real uid of the current process (std has no API for this; on unix obtained
/// via `/proc` metadata to avoid pulling in libc). Windows has no uid concept —
/// the per-uid `/tmp/asd-<uid>` socket path is unix-only anyway (the Windows
/// client is GUI-only and reaches daemons over `$ASD_SOCKET`/remotes), so 0 is
/// a harmless placeholder that lets the crate compile there.
#[cfg(unix)]
fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(0)
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
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

    #[test]
    fn session_name_rules() {
        assert!(is_valid_session_name("s0"));
        assert!(is_valid_session_name("work_2026-07"));
        assert!(is_valid_session_name(&"a".repeat(64)));
        assert!(!is_valid_session_name(""));
        assert!(!is_valid_session_name(&"a".repeat(65)));
        assert!(!is_valid_session_name("has space"));
        assert!(!is_valid_session_name("中文"));
        assert!(!is_valid_session_name("dot.dot"));
    }
}
