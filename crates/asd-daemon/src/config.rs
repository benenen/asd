//! Daemon configuration, loaded from `~/.config/asd/config.toml` (the path is
//! resolved by [`asd_proto::paths::config_path`]).
//!
//! Today there is a single knob — the per-session scrollback depth — but the
//! shape (`[session]` table, every field optional) is chosen so more can be
//! added later without breaking existing files, and so a partial or absent file
//! simply falls back to defaults. A broken config never stops the daemon: it
//! logs a warning and serves with defaults.

use std::path::Path;

use serde::Deserialize;
use tracing::{info, warn};

/// Default scrollback depth (lines) when the config is absent or omits it.
/// Matches the historical hard-coded value.
pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// Resolved daemon configuration, with defaults already applied.
#[derive(Debug, Clone)]
pub struct Config {
    /// Lines of scrollback history each session's terminal keeps.
    pub scrollback_lines: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scrollback_lines: DEFAULT_SCROLLBACK_LINES,
        }
    }
}

/// On-disk shape. Every field is optional so a partial file (or an empty one)
/// merges onto the defaults rather than failing. Unknown keys are ignored for
/// forward compatibility (an older daemon must tolerate a newer file).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    session: RawSession,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawSession {
    scrollback_lines: Option<usize>,
}

impl From<RawConfig> for Config {
    fn from(raw: RawConfig) -> Self {
        let defaults = Config::default();
        Self {
            scrollback_lines: raw
                .session
                .scrollback_lines
                .unwrap_or(defaults.scrollback_lines),
        }
    }
}

impl Config {
    /// Load and resolve the config at `path`. A missing file yields defaults; a
    /// present-but-unreadable or unparsable file logs a warning and *also* falls
    /// back to defaults — a bad config must never keep the daemon from serving.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let cfg = Self::parse(&text);
                info!(
                    path = %path.display(),
                    scrollback_lines = cfg.scrollback_lines,
                    "config loaded"
                );
                cfg
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "reading config failed; using defaults");
                Self::default()
            }
        }
    }

    /// Parse TOML text into a resolved `Config`, falling back to defaults (with a
    /// warning) on a parse error. Split out from [`load`] so it is unit-testable
    /// without touching the filesystem.
    fn parse(text: &str) -> Self {
        match toml::from_str::<RawConfig>(text) {
            Ok(raw) => Self::from(raw),
            Err(e) => {
                warn!(error = %e, "parsing config failed; using defaults");
                Self::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_all_defaults() {
        assert_eq!(Config::parse("").scrollback_lines, DEFAULT_SCROLLBACK_LINES);
    }

    #[test]
    fn scrollback_lines_is_read() {
        assert_eq!(
            Config::parse("[session]\nscrollback_lines = 500").scrollback_lines,
            500
        );
        // Zero is a legitimate "no scrollback" choice, not a fall-through.
        assert_eq!(
            Config::parse("[session]\nscrollback_lines = 0").scrollback_lines,
            0
        );
    }

    #[test]
    fn missing_field_or_table_falls_back() {
        // Table present, field absent.
        assert_eq!(
            Config::parse("[session]").scrollback_lines,
            DEFAULT_SCROLLBACK_LINES
        );
        // Unknown keys are ignored, not fatal (forward compatibility).
        assert_eq!(
            Config::parse("nonsense = true\n[session]\nscrollback_lines = 42").scrollback_lines,
            42
        );
    }

    #[test]
    fn invalid_toml_uses_defaults() {
        // A wrong type / malformed value must not crash — fall back to defaults.
        assert_eq!(
            Config::parse("[session]\nscrollback_lines = \"lots\"").scrollback_lines,
            DEFAULT_SCROLLBACK_LINES
        );
        assert_eq!(
            Config::parse("this is not toml {{{").scrollback_lines,
            DEFAULT_SCROLLBACK_LINES
        );
    }

    #[test]
    fn missing_file_is_defaults() {
        let cfg = Config::load(Path::new("/nonexistent/asd/does-not-exist.toml"));
        assert_eq!(cfg.scrollback_lines, DEFAULT_SCROLLBACK_LINES);
    }
}
