//! TUI configuration, read from the same `config.toml` the daemon reads (the
//! path is resolved by [`asd_proto::paths::config_path`]).
//!
//! Only the `[keys]` table belongs to the TUI; the daemon owns `[session]` and
//! ignores this one, as this ignores that. Everything is optional, so a partial
//! file — or none at all — means the defaults in [`crate::keymap`].
//!
//! A config that will not compile never locks anyone out of their sessions: the
//! defaults take over and the reason is shown as a notice, the same way the
//! daemon logs and serves on.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use tracing::warn;

use crate::keymap::{KeyAction, KeyBindingOverride, Keymap, KeymapOverrides};

/// On-disk shape. Unknown tables are ignored so a newer file does not stop an
/// older client — and so the daemon's own tables pass straight through.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    keys: RawKeys,
}

/// `[keys]`: the leader, plus a rebind table for each context. The action names
/// are [`KeyAction::from_config_name`]; each maps to the list of keys that
/// should invoke it, *replacing* the defaults for that action rather than
/// adding to them.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawKeys {
    leader: Option<String>,
    direct: BTreeMap<String, Vec<String>>,
    prefix: BTreeMap<String, Vec<String>>,
}

/// The keymap for this session, and the complaint to show if the file had one.
pub(crate) fn keymap(path: &Path) -> (Keymap, Option<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (Keymap::default(), None);
        }
        Err(error) => {
            warn!(path = %path.display(), %error, "reading config failed; using default keys");
            return (
                Keymap::default(),
                Some(format!("config unreadable ({error}) — default keys")),
            );
        }
    };
    from_text(&text)
}

/// Split out from [`keymap`] so the whole resolution is testable without
/// touching the filesystem.
fn from_text(text: &str) -> (Keymap, Option<String>) {
    let raw = match toml::from_str::<RawConfig>(text) {
        Ok(raw) => raw,
        Err(error) => {
            warn!(%error, "parsing config failed; using default keys");
            return (
                Keymap::default(),
                Some(format!("config unparsable ({error}) — default keys")),
            );
        }
    };
    let (overrides, unknown) = overrides(raw.keys);
    match Keymap::configured(overrides) {
        Ok(keymap) if unknown.is_empty() => (keymap, None),
        // The bindings that *were* understood still apply; naming an action
        // that does not exist is almost always a typo, and silently doing
        // nothing about it is the one outcome that teaches nothing.
        Ok(keymap) => (
            keymap,
            Some(format!("config: no such key action {}", unknown.join(", "))),
        ),
        Err(errors) => {
            warn!(%errors, "key bindings rejected; using default keys");
            (
                Keymap::default(),
                Some(format!("config keys: {errors} — default keys")),
            )
        }
    }
}

fn overrides(keys: RawKeys) -> (KeymapOverrides, Vec<String>) {
    let mut unknown = Vec::new();
    let direct = rebinds("keys.direct", keys.direct, &mut unknown);
    let prefix = rebinds("keys.prefix", keys.prefix, &mut unknown);
    (
        KeymapOverrides {
            leader: keys.leader,
            direct,
            prefix,
        },
        unknown,
    )
}

fn rebinds(
    table: &str,
    raw: BTreeMap<String, Vec<String>>,
    unknown: &mut Vec<String>,
) -> Vec<KeyBindingOverride> {
    raw.into_iter()
        .filter_map(|(name, aliases)| match KeyAction::from_config_name(&name) {
            Some(action) => Some(KeyBindingOverride { action, aliases }),
            None => {
                unknown.push(format!("{table}.{name}"));
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
