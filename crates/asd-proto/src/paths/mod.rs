//! Naming and path contract (spec §2).
//!
//! The daemon and all clients share this single convention; this module does
//! pure path computation only — directory creation (including 0700
//! permissions) is the responsibility of the daemon/spawner.
//!
//! Everything that resolves differently per platform lives in `unix.rs`/
//! `win.rs`, both mounted here as `imp`. The two `cfg`s below are the only
//! ones in this module: the re-export itself is unconditional, so a platform
//! that fails to provide the whole surface does not compile — the two
//! implementations cannot silently drift apart.

use std::path::PathBuf;

#[cfg(unix)]
#[path = "unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "win.rs"]
mod imp;

use imp::config_dir;
pub use imp::{data_dir, pid_path, socket_path};

/// Maximum session name length.
pub const SESSION_NAME_MAX: usize = 64;

/// Session name contract: `[A-Za-z0-9_-]{1,64}`.
pub fn is_valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= SESSION_NAME_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Path of the persisted session list: `<data_dir>/sessions.tsv`. The daemon
/// rewrites it on every session create/rename/kill and restores from it on every
/// startup. Lives in the (persistent) data directory, keyed by it — a single
/// daemon per data directory. Read-write daemon state, distinct from the
/// read-only user `config.toml`.
pub fn session_list_path() -> PathBuf {
    data_dir().join("sessions.tsv")
}

/// Config file: `<config_dir>/config.toml`. `ASD_CONFIG` overrides it entirely
/// (tests, multi-instance). The daemon reads it once at startup; it is never
/// auto-created — a missing file just means "all defaults".
pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("ASD_CONFIG")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    config_dir().join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_list_path_is_sessions_tsv_in_data_dir() {
        let p = session_list_path();
        assert_eq!(p.file_name().unwrap(), std::ffi::OsStr::new("sessions.tsv"));
        assert_eq!(p.parent().unwrap(), data_dir());
    }

    /// A hand-edited config.toml must never sit among the files the daemon
    /// rewrites. This held by convention on unix and was briefly violated on
    /// Windows (both resolved to `%USERPROFILE%\asd`), so pin it.
    #[test]
    fn config_and_data_never_share_a_directory() {
        // An explicit ASD_CONFIG puts the file wherever the user asked, so the
        // invariant only applies to the derived location.
        if std::env::var_os("ASD_CONFIG").is_some_and(|v| !v.is_empty()) {
            return;
        }
        assert_ne!(config_path().parent().unwrap(), data_dir());
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
