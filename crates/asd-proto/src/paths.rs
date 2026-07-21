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

/// Daemon data directory: `~/.local/share/asd/` (session metadata, logs).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("asd");
    }
    home_dir().join(".local/share/asd")
}

/// Path of the persisted session list: `<data_dir>/sessions.tsv`. The daemon
/// rewrites it on every session create/rename/kill and restores from it on every
/// startup. Lives in the (persistent) data directory, keyed by it — a single
/// daemon per data directory. Read-write daemon state, distinct from the
/// read-only user `config.toml`.
pub fn session_list_path() -> PathBuf {
    data_dir().join("sessions.tsv")
}

/// Config file: `$XDG_CONFIG_HOME/asd/config.toml`, falling back to
/// `~/.config/asd/config.toml`. `ASD_CONFIG` overrides it entirely (tests,
/// multi-instance). The daemon reads it once at startup; it is never
/// auto-created — a missing file just means "all defaults".
pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("ASD_CONFIG")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    config_dir().join("config.toml")
}

/// Directory holding the config file: `$XDG_CONFIG_HOME/asd`, falling back to
/// `~/.config/asd`.
fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("asd");
    }
    home_dir().join(".config/asd")
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
    fn session_list_path_is_sessions_tsv_in_data_dir() {
        let p = session_list_path();
        assert_eq!(p.file_name().unwrap(), std::ffi::OsStr::new("sessions.tsv"));
        assert_eq!(p.parent().unwrap(), data_dir());
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
