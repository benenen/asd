//! The on-disk (and embedded) shape of one agent's detection manifest: which
//! part of the screen to look at, what to look for there, and which state that
//! means.
//!
//! Everything here is data — the engine in [`super`] evaluates it. Keeping the
//! rules out of Rust is the point: an agent's UI changes on its own schedule,
//! so a user must be able to fix a rule by dropping a file in, not by waiting
//! for a release.
//!
//! Validation happens during deserialization (regions and character ranges
//! parse into enums), so a broken file is rejected with a message naming the
//! offending value instead of silently matching nothing. Unknown keys are
//! ignored, matching the daemon config's forward-compatibility rule: an older
//! daemon must tolerate a newer file.

use serde::Deserialize;

use super::AgentState;

/// Vocabulary version of this engine: the set of regions and predicates below.
/// A manifest declares the oldest engine it can run on (`min_engine_version`);
/// one asking for more than this is skipped with a warning rather than
/// half-understood.
pub const ENGINE_VERSION: u32 = 1;

/// One agent's rule set.
#[derive(Debug, Deserialize)]
pub struct Manifest {
    /// Agent identity, matched against the session's foreground command.
    pub id: String,
    /// Rule-set version, for the human comparing two copies of the same file.
    /// Free-form and never interpreted.
    #[serde(default)]
    pub version: String,
    /// Other command names that mean this agent.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Oldest engine this manifest can run on.
    #[serde(default = "default_min_engine_version")]
    pub min_engine_version: u32,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

fn default_min_engine_version() -> u32 {
    1
}

impl Manifest {
    /// Whether `id` names this agent (its own id or one of its aliases).
    pub fn matches_agent(&self, id: &str) -> bool {
        self.id == id || self.aliases.iter().any(|a| a == id)
    }
}

/// One rule: a state claim about a region of the screen, guarded by a
/// predicate and ranked by priority.
#[derive(Debug, Deserialize)]
pub struct Rule {
    /// Stable name, used in tests and in the daemon's trace output. Never
    /// matched against anything.
    pub id: String,
    /// What a match means. `unknown` is a real answer: it marks a screen the
    /// rules recognize but cannot classify (a transcript viewer, a settings
    /// menu), so a lower-priority rule does not get to guess at it.
    pub state: AgentState,
    /// Higher wins. Ties go to the earlier rule in the file.
    #[serde(default)]
    pub priority: i32,
    pub region: Region,
    #[serde(flatten)]
    pub predicate: Predicate,
}

/// Which part of the screen a rule reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum Region {
    /// The terminal title (OSC 0/2) as a single line. Agents put their spinner
    /// there, which makes it the cheapest and most reliable signal available.
    OscTitle,
    /// The last N non-empty lines of the screen, in screen order. Non-empty
    /// because an agent's prompt box floats above a variable run of blanks.
    BottomNonEmptyLines(usize),
    /// Every line of the visible screen.
    WholeScreen,
}

impl TryFrom<String> for Region {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let value = value.trim();
        if value == "osc_title" {
            return Ok(Self::OscTitle);
        }
        if value == "whole_screen" {
            return Ok(Self::WholeScreen);
        }
        if let Some(arg) = value
            .strip_prefix("bottom_non_empty_lines(")
            .and_then(|rest| rest.strip_suffix(')'))
        {
            let n: usize = arg
                .trim()
                .parse()
                .map_err(|_| format!("bottom_non_empty_lines wants a count, got {arg:?}"))?;
            if n == 0 {
                return Err("bottom_non_empty_lines(0) can never match".to_string());
            }
            return Ok(Self::BottomNonEmptyLines(n));
        }
        Err(format!(
            "unknown region {value:?} (want osc_title, whole_screen, \
             or bottom_non_empty_lines(N))"
        ))
    }
}

/// An inclusive range of scalar values, written as hex: `"2800-28ff"`, or
/// `"2733"` for a single one. Spinner glyphs are what this exists for — they
/// are a *class* of characters an agent cycles through, so listing them
/// individually would rot the moment it added one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct CharRange {
    start: char,
    end: char,
}

impl CharRange {
    pub fn contains(&self, c: char) -> bool {
        (self.start..=self.end).contains(&c)
    }
}

impl TryFrom<String> for CharRange {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let value = value.trim();
        let (start, end) = match value.split_once('-') {
            Some((a, b)) => (a, b),
            None => (value, value),
        };
        let parse = |s: &str| -> Result<char, String> {
            let n = u32::from_str_radix(s.trim(), 16)
                .map_err(|_| format!("{s:?} is not a hex scalar value"))?;
            char::from_u32(n).ok_or_else(|| format!("{s:?} is not a character"))
        };
        let (start, end) = (parse(start)?, parse(end)?);
        if start > end {
            return Err(format!("range {value:?} runs backwards"));
        }
        Ok(Self { start, end })
    }
}

/// What has to hold for a rule to fire. Every field that is present must hold
/// — they are `and`-ed — and an entirely empty predicate matches *nothing*.
///
/// That last part is deliberate: a rule whose conditions were all dropped (or
/// misspelled, since unknown keys are ignored) would otherwise match every
/// screen and, at a high priority, pin every session to one state.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Predicate {
    /// Every string appears somewhere in the region.
    pub contains: Vec<String>,
    /// Each entry is satisfied by some *single* line. Two conditions that must
    /// hold together — a spinner glyph and the elapsed time beside it — belong
    /// in one entry; spread across two they could be met by two unrelated
    /// lines, which is how a transcript full of ordinary prose starts looking
    /// like a status bar.
    pub line: Vec<LinePredicate>,
    /// At least one of these holds.
    pub any: Vec<Predicate>,
    /// None of these hold.
    pub not: Vec<Predicate>,
}

impl Predicate {
    fn is_empty(&self) -> bool {
        self.contains.is_empty()
            && self.line.is_empty()
            && self.any.is_empty()
            && self.not.is_empty()
    }

    /// Evaluate against a region the caller has already lowercased (once per
    /// region, rather than once per predicate). Match strings are lowercased at
    /// the same point, so a manifest is written in whatever case reads best.
    pub fn matches(&self, region: &RegionText) -> bool {
        if self.is_empty() {
            return false;
        }
        if !self
            .contains
            .iter()
            .all(|needle| region.joined.contains(&needle.to_lowercase()))
        {
            return false;
        }
        if !self
            .line
            .iter()
            .all(|line| region.lines.iter().any(|text| line.matches(text)))
        {
            return false;
        }
        if !self.any.is_empty() && !self.any.iter().any(|p| p.matches(region)) {
            return false;
        }
        if self.not.iter().any(|p| p.matches(region)) {
            return false;
        }
        true
    }
}

/// Conditions on one line. All present fields must hold on the *same* line; an
/// empty one matches nothing, for the same reason an empty [`Predicate`] does.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LinePredicate {
    /// The line, ignoring leading whitespace, starts with one of these.
    pub starts_with: Vec<String>,
    /// Its first non-whitespace character falls in one of these ranges.
    pub first_char_in: Vec<CharRange>,
    /// It contains all of these.
    pub contains: Vec<String>,
}

impl LinePredicate {
    fn matches(&self, line: &str) -> bool {
        if self.starts_with.is_empty() && self.first_char_in.is_empty() && self.contains.is_empty()
        {
            return false;
        }
        let trimmed = line.trim_start();
        if !self.starts_with.is_empty()
            && !self
                .starts_with
                .iter()
                .any(|p| trimmed.starts_with(&p.to_lowercase()))
        {
            return false;
        }
        if !self.first_char_in.is_empty()
            && !trimmed
                .chars()
                .next()
                .is_some_and(|c| self.first_char_in.iter().any(|r| r.contains(c)))
        {
            return false;
        }
        self.contains
            .iter()
            .all(|needle| line.contains(&needle.to_lowercase()))
    }
}

/// A region's text, lowercased once and kept in both shapes the predicates
/// need: line by line, and joined. The join uses `\n` so a `contains` cannot
/// match across a line break.
#[derive(Debug)]
pub struct RegionText {
    pub lines: Vec<String>,
    pub joined: String,
}

impl RegionText {
    pub fn new(lines: Vec<String>) -> Self {
        let lines: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
        let joined = lines.join("\n");
        Self { lines, joined }
    }
}
