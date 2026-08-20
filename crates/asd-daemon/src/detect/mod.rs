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
//!
//! Prior art: herdr (<https://github.com/herdrdev/herdr>, Apache-2.0) reads
//! agent state off the terminal the same way and keeps its rules in per-agent
//! TOML. This module follows that design, and Claude's rule set in particular
//! owes it the ranking it uses and several of the strings it keys on. What is
//! here is not a port: the predicates are re-expressed without regex, the
//! region vocabulary is smaller, and every shipped rule is justified by a
//! capture checked into `fixtures/` rather than carried over on trust.

mod manifest;

use std::path::Path;

use tracing::{info, warn};

/// The state vocabulary is the protocol's: the daemon is the only thing that
/// may set it, and every client reads the same enum off the wire.
pub use asd_proto::AgentState;
pub use manifest::{ENGINE_VERSION, Manifest, Region, RegionText, Rule};

/// Rule sets shipped with the binary. A user file of the same `id` replaces
/// the embedded one wholesale — merging two rule sets by priority would make
/// it impossible to *remove* a rule that has started firing wrongly, which is
/// the whole reason to reach for an override.
const EMBEDDED: &[&str] = &[
    include_str!("manifests/claude.toml"),
    include_str!("manifests/codex.toml"),
    include_str!("manifests/opencode.toml"),
    include_str!("manifests/pi.toml"),
];

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

    /// The state `command`'s screen says it is in, and the rule that said so.
    /// `command` is the session's foreground command as `SessionInfo.command`
    /// reports it; an agent with no manifest, or a screen no rule claims, is
    /// [`AgentState::Unknown`] with no rule.
    ///
    /// The rule comes back with the verdict because a state nobody expected is
    /// only debuggable if the daemon can say what produced it.
    pub(crate) fn detect(&self, command: &str, screen: &Screen<'_>) -> (AgentState, Option<&Rule>) {
        let rule = self.matching_rule(command, screen);
        (rule.map(|r| r.state).unwrap_or_default(), rule)
    }

    /// The winning rule. Highest priority wins; ties go to the earlier rule in
    /// the file.
    pub(crate) fn matching_rule(&self, command: &str, screen: &Screen<'_>) -> Option<&Rule> {
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
