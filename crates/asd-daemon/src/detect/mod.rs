//! Screen-derived agent state: what the program in a session is *doing*, read
//! off its rendered screen instead of off byte activity.
//!
//! `SessionInfo.running` answers a different question — whether bytes are
//! arriving — and cannot tell a busy-but-silent program from one that has
//! stopped to ask the user something. For an AI agent that second case is the
//! one worth surfacing: `Blocked` is the state a person has to act on. The two
//! stay separate rather than being folded together, so `wait --idle` keeps
//! meaning exactly what it means today.
//!
//! Rules live in TOML ([`manifest`]), embedded per agent and overridable from
//! the user's config directory. Detection is pure — screen text in, state out —
//! so it is testable against captured screens without a pty in sight.

// Nothing in the daemon calls this yet: the engine and its rules land first,
// verified against captured screens, and the session thread starts asking it
// for a state in the step that adds `SessionInfo.state` (a protocol change).
// Remove this the moment that call site exists.
#![allow(dead_code)]

mod manifest;

use std::path::Path;

use serde::Deserialize;
use tracing::{info, warn};

pub use manifest::{ENGINE_VERSION, Manifest, Region, RegionText, Rule};

/// Rule sets shipped with the binary. A user file of the same `id` replaces
/// the embedded one wholesale — merging two rule sets by priority would make
/// it impossible to *remove* a rule that has started firing wrongly, which is
/// the whole reason to reach for an override.
const EMBEDDED: &[&str] = &[
    include_str!("manifests/claude.toml"),
    include_str!("manifests/codex.toml"),
];

/// What the program in a session is doing, as read from its screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    /// Busy on a turn of its own — a spinner, an "esc to interrupt" hint.
    Working,
    /// Stopped, waiting for a person: a permission prompt, a question, a
    /// selection. The state a session list exists to surface.
    Blocked,
    /// Ready for input, with nothing pending.
    Idle,
    /// No agent recognized, or a screen the rules deliberately decline to
    /// classify. Never a claim that something *is* finished.
    #[default]
    Unknown,
}

/// The screen a detection runs against.
pub struct Screen<'a> {
    /// The terminal title (OSC 0/2), empty when never set.
    pub title: &'a str,
    /// The visible screen as plain text, top row first.
    pub lines: &'a [String],
}

impl Screen<'_> {
    /// The lines a region selects, in screen order.
    fn region(&self, region: Region) -> Vec<String> {
        match region {
            Region::OscTitle => vec![self.title.to_string()],
            Region::WholeScreen => self.lines.to_vec(),
            Region::BottomNonEmptyLines(n) => {
                let mut picked: Vec<String> = self
                    .lines
                    .iter()
                    .rev()
                    .filter(|l| !l.trim().is_empty())
                    .take(n)
                    .cloned()
                    .collect();
                picked.reverse();
                picked
            }
        }
    }
}

/// The loaded rule sets.
pub struct Detector {
    manifests: Vec<Manifest>,
}

impl Detector {
    /// The embedded rule sets, with any file in `overrides` replacing the
    /// embedded manifest of the same `id`. A missing directory is not an
    /// error — it is the normal case.
    ///
    /// A file that fails to parse is skipped with a warning and the embedded
    /// copy stands. Detection is a display nicety; a typo in a hand-edited
    /// rule file must never keep the daemon from serving sessions.
    pub fn load(overrides: Option<&Path>) -> Self {
        let mut manifests: Vec<Manifest> = EMBEDDED
            .iter()
            .filter_map(|text| match toml::from_str::<Manifest>(text) {
                Ok(m) => Some(m),
                // An embedded manifest is compiled in from this repository, so
                // this is a bug rather than user input — loud, but still not
                // fatal to the daemon.
                Err(e) => {
                    warn!(error = %e, "embedded agent manifest is invalid; skipping");
                    None
                }
            })
            .collect();

        if let Some(dir) = overrides {
            for text in read_manifest_dir(dir) {
                match toml::from_str::<Manifest>(&text) {
                    Ok(m) => {
                        info!(
                            agent = %m.id,
                            version = %m.version,
                            "agent manifest loaded from the config directory"
                        );
                        manifests.retain(|existing| existing.id != m.id);
                        manifests.push(m);
                    }
                    Err(e) => warn!(error = %e, "agent manifest is invalid; keeping the built-in"),
                }
            }
        }

        manifests.retain(|m| {
            let ok = m.min_engine_version <= ENGINE_VERSION;
            if !ok {
                warn!(
                    agent = %m.id,
                    wants = m.min_engine_version,
                    have = ENGINE_VERSION,
                    "agent manifest needs a newer detection engine; skipping"
                );
            }
            ok
        });

        Self { manifests }
    }

    /// The state `command`'s screen says it is in. `command` is the session's
    /// foreground command as `SessionInfo.command` reports it; an agent with no
    /// manifest, or a screen no rule claims, is [`AgentState::Unknown`].
    pub fn detect(&self, command: &str, screen: &Screen<'_>) -> AgentState {
        self.matching_rule(command, screen)
            .map(|rule| rule.state)
            .unwrap_or_default()
    }

    /// The winning rule, for tests and tracing. Highest priority wins; ties go
    /// to the earlier rule in the file.
    fn matching_rule(&self, command: &str, screen: &Screen<'_>) -> Option<&Rule> {
        let id = agent_id(command)?;
        let manifest = self.manifests.iter().find(|m| m.matches_agent(&id))?;

        // One region is usually read by several rules; lowercasing it once per
        // distinct region keeps a screenful of rules to a handful of passes.
        let mut cache: Vec<(Region, RegionText)> = Vec::new();
        let mut best: Option<&Rule> = None;
        for rule in &manifest.rules {
            if best.is_some_and(|b| b.priority >= rule.priority) {
                continue;
            }
            let text = match cache.iter().position(|(r, _)| *r == rule.region) {
                Some(i) => &cache[i].1,
                None => {
                    cache.push((rule.region, RegionText::new(screen.region(rule.region))));
                    &cache.last().expect("just pushed").1
                }
            };
            if rule.predicate.matches(text) {
                best = Some(rule);
            }
        }
        best
    }
}

/// Interpreters that run an agent rather than being one. A command starting
/// with one of these names the agent in a later word, so the first word is the
/// wrong answer: Codex ships as `node .../bin/codex`, and reading that as
/// `node` would leave every Codex session unrecognized.
const INTERPRETERS: &[&str] = &[
    "node", "nodejs", "bun", "deno", "python", "python3", "py", "ruby", "perl", "uv", "uvx", "npx",
    "pnpm", "yarn", "bunx",
];

/// The agent name inside a foreground command, lowercased and stripped of its
/// directory and any `.exe`: `/opt/bin/claude --resume` → `claude`, and
/// `node /root/.nvm/versions/node/v24.16.0/bin/codex` → `codex`.
///
/// Flags are skipped along the way (`node --enable-source-maps .../codex`), but
/// only while looking past an interpreter — the first word of an ordinary
/// command is taken as given.
fn agent_id(command: &str) -> Option<String> {
    let mut words = command.split_whitespace();
    let mut name = basename(words.next()?)?;
    while INTERPRETERS.contains(&name.as_str()) {
        let next = words.find(|w| !w.starts_with('-'))?;
        name = basename(next)?;
    }
    Some(name)
}

/// A path's final component, without an `.exe`, lowercased.
fn basename(word: &str) -> Option<String> {
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word);
    let base = base.strip_suffix(".exe").unwrap_or(base);
    (!base.is_empty()).then(|| base.to_lowercase())
}

/// Every `*.toml` in `dir`, sorted by name so two files claiming one `id`
/// resolve the same way on every start. Unreadable entries are skipped.
fn read_manifest_dir(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    paths.sort();
    paths
        .iter()
        .filter_map(|p| match std::fs::read_to_string(p) {
            Ok(text) => Some(text),
            Err(e) => {
                warn!(path = %p.display(), error = %e, "reading agent manifest failed");
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
