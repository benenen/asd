//! Declarative TUI key bindings: one source of truth for routing and hints.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

mod key_spec;

use key_spec::{KeyPattern, parse_leader};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum KeyAction {
    SelectNext,
    SelectPrevious,
    JumpTo(u16),
    Create,
    ToggleSidebar,
    ToggleStatus,
    Kill,
    Rename,
    ToggleGitGraph,
    Reconnect,
    Quit,
    ScrollPageUp,
    ScrollPageDown,
    CancelPrefix,
    SendLeaderLiteral,
}

/// The names `config.toml` gives the actions it can rebind, paired with the
/// action itself so the two cannot drift.
///
/// `JumpTo` and `SendLeaderLiteral` are deliberately absent. The first is nine
/// bindings the sidebar prints ordinals for, so rebinding one of them would
/// make a row lie; the second is the leader pressed twice, which follows
/// whatever the leader is set to.
const CONFIG_NAMES: &[(&str, KeyAction)] = &[
    ("select_next", KeyAction::SelectNext),
    ("select_previous", KeyAction::SelectPrevious),
    ("create", KeyAction::Create),
    ("rename", KeyAction::Rename),
    ("kill", KeyAction::Kill),
    ("toggle_sidebar", KeyAction::ToggleSidebar),
    ("toggle_status", KeyAction::ToggleStatus),
    ("toggle_git_graph", KeyAction::ToggleGitGraph),
    ("reconnect", KeyAction::Reconnect),
    ("quit", KeyAction::Quit),
    ("scroll_page_up", KeyAction::ScrollPageUp),
    ("scroll_page_down", KeyAction::ScrollPageDown),
    ("cancel_prefix", KeyAction::CancelPrefix),
];

impl KeyAction {
    /// The action `name` stands for in `config.toml`, if it names one at all.
    pub(crate) fn from_config_name(name: &str) -> Option<Self> {
        CONFIG_NAMES
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, action)| *action)
    }

    /// What `config.toml` calls this action. Used to report a rebind that
    /// named an action the context does not have.
    pub(crate) fn config_name(self) -> &'static str {
        CONFIG_NAMES
            .iter()
            .find(|(_, candidate)| *candidate == self)
            .map_or("(unnameable)", |(name, _)| *name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyResolution {
    PassThrough,
    Consumed,
    Action(KeyAction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyHint {
    pub text: String,
    pub prefix_active: bool,
}

#[derive(Debug, Clone)]
struct Binding {
    aliases: Vec<KeyPattern>,
    action: KeyAction,
    description: &'static str,
}

#[derive(Debug, Clone)]
struct KeymapSpec {
    leader: KeyPattern,
    direct: Vec<Binding>,
    prefix: Vec<Binding>,
}

#[derive(Debug, Clone)]
pub(crate) struct KeyBindingOverride {
    pub action: KeyAction,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct KeymapOverrides {
    pub leader: Option<String>,
    pub direct: Vec<KeyBindingOverride>,
    pub prefix: Vec<KeyBindingOverride>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeymapError {
    InvalidKeySpec {
        value: String,
        reason: String,
    },
    EmptyAliases {
        action: KeyAction,
    },
    DuplicateAlias {
        context: &'static str,
        key: String,
        action: KeyAction,
    },
    UnknownAction {
        action: KeyAction,
    },
    ConflictingBinding {
        context: &'static str,
        key: String,
        first: KeyAction,
        second: KeyAction,
    },
    LeaderConflict {
        context: &'static str,
        key: String,
        action: KeyAction,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeymapErrors(pub(crate) Vec<KeymapError>);

impl std::fmt::Display for KeymapErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}

impl std::fmt::Display for KeymapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidKeySpec { value, reason } => {
                write!(f, "invalid key '{value}': {reason}")
            }
            Self::EmptyAliases { action } => write!(f, "{action:?} has no keys"),
            Self::DuplicateAlias {
                context,
                key,
                action,
            } => write!(f, "{context} key {key} is repeated for {action:?}"),
            Self::UnknownAction { action } => {
                write!(f, "{} is not bound in this context", action.config_name())
            }
            Self::ConflictingBinding {
                context,
                key,
                first,
                second,
            } => write!(
                f,
                "{context} key {key} is bound to both {first:?} and {second:?}"
            ),
            Self::LeaderConflict {
                context,
                key,
                action,
            } => write!(
                f,
                "{context} key {key} for {action:?} conflicts with the leader"
            ),
        }
    }
}

impl std::error::Error for KeymapErrors {}

impl Binding {
    fn fixed(aliases: Vec<KeyPattern>, action: KeyAction, description: &'static str) -> Self {
        Self {
            aliases,
            action,
            description,
        }
    }

    fn resolve(&self, key: &KeyEvent) -> Option<KeyAction> {
        self.aliases
            .iter()
            .any(|alias| alias.matches(key))
            .then_some(self.action)
    }

    fn hint(&self) -> String {
        let keys = self
            .aliases
            .iter()
            .map(KeyPattern::label)
            .collect::<Vec<_>>()
            .join("/");
        format!("{keys} {}", self.description)
    }
}

fn binding_hints(bindings: &[Binding]) -> Vec<String> {
    let mut hints = Vec::new();
    let mut index = 0;
    while index < bindings.len() {
        if matches!(bindings[index].action, KeyAction::JumpTo(_)) {
            let end = bindings[index..]
                .iter()
                .position(|binding| !matches!(binding.action, KeyAction::JumpTo(_)))
                .map_or(bindings.len(), |offset| index + offset);
            hints.push(jump_hint(&bindings[index..end]));
            index = end;
        } else {
            hints.push(bindings[index].hint());
            index += 1;
        }
    }
    hints
}

fn jump_hint(bindings: &[Binding]) -> String {
    let labels = bindings
        .iter()
        .filter_map(|binding| binding.aliases.first().map(KeyPattern::label))
        .collect::<Vec<_>>();
    let compact = if labels.len() > 1
        && labels
            .windows(2)
            .all(|pair| consecutive_char_labels(&pair[0], &pair[1]))
    {
        format!("{}-{}", labels[0], labels[labels.len() - 1])
    } else {
        labels.join("/")
    };
    format!("{compact} jump")
}

fn consecutive_char_labels(left: &str, right: &str) -> bool {
    let mut left = left.chars();
    let mut right = right.chars();
    matches!(
        (left.next(), left.next(), right.next(), right.next()),
        (Some(left), None, Some(right), None) if left as u32 + 1 == right as u32
    )
}

#[derive(Debug, Clone)]
pub(crate) struct Keymap {
    leader: KeyPattern,
    direct: Vec<Binding>,
    prefix: Vec<Binding>,
    prefix_active: bool,
}

impl Default for KeymapSpec {
    fn default() -> Self {
        let patterns = |values: &[&str]| {
            values
                .iter()
                .map(|value| KeyPattern::parse(value))
                .collect::<Result<Vec<_>, _>>()
                .expect("default key names must be valid")
        };
        let direct = vec![
            // Switching sessions is the one thing frequent enough to be worth a
            // chord of its own: the prefix costs two keystrokes every time, and
            // walking a sidebar of twenty is where that adds up. Ctrl+Alt is
            // free of the leader and of anything a shell reads, but some
            // desktops bind Ctrl+Alt+arrows to workspace switching — which is
            // what `[keys.direct]` in config.toml is for.
            Binding::fixed(patterns(&["Ctrl+Alt+Down"]), KeyAction::SelectNext, "next"),
            Binding::fixed(
                patterns(&["Ctrl+Alt+Up"]),
                KeyAction::SelectPrevious,
                "previous",
            ),
            Binding::fixed(
                patterns(&["Shift+PageUp"]),
                KeyAction::ScrollPageUp,
                "scroll up",
            ),
            Binding::fixed(
                patterns(&["Shift+PageDown"]),
                KeyAction::ScrollPageDown,
                "scroll down",
            ),
        ];
        let prefix = vec![
            Binding::fixed(patterns(&["j", "Down"]), KeyAction::SelectNext, "next"),
            Binding::fixed(
                patterns(&["k", "Up"]),
                KeyAction::SelectPrevious,
                "previous",
            ),
        ]
        .into_iter()
        .chain(('1'..='9').zip(1_u16..=9).map(|(key, ordinal)| {
            Binding::fixed(
                vec![KeyPattern::code(KeyCode::Char(key))],
                KeyAction::JumpTo(ordinal),
                "jump",
            )
        }))
        .chain([
            Binding::fixed(patterns(&["c"]), KeyAction::Create, "new"),
            Binding::fixed(patterns(&["r"]), KeyAction::Rename, "rename"),
            Binding::fixed(patterns(&["x"]), KeyAction::Kill, "kill"),
            Binding::fixed(patterns(&["b"]), KeyAction::ToggleSidebar, "sidebar"),
            Binding::fixed(patterns(&["s"]), KeyAction::ToggleStatus, "status"),
            Binding::fixed(patterns(&["g"]), KeyAction::ToggleGitGraph, "git graph"),
            Binding::fixed(patterns(&["R"]), KeyAction::Reconnect, "reconnect"),
            Binding::fixed(patterns(&["q"]), KeyAction::Quit, "quit"),
            Binding::fixed(patterns(&["Esc"]), KeyAction::CancelPrefix, "cancel"),
        ])
        .collect();
        Self {
            leader: parse_leader("Ctrl+A").expect("default leader must be valid"),
            direct,
            prefix,
        }
    }
}

impl KeymapSpec {
    fn with_overrides(self, overrides: KeymapOverrides) -> Result<Self, KeymapError> {
        let spec = if let Some(leader) = overrides.leader {
            Self {
                leader: parse_leader(&leader)?,
                ..self
            }
        } else {
            self
        };
        let spec = replace_bindings(spec, true, overrides.direct)?;
        replace_bindings(spec, false, overrides.prefix)
    }
}

fn replace_bindings(
    spec: KeymapSpec,
    direct: bool,
    overrides: Vec<KeyBindingOverride>,
) -> Result<KeymapSpec, KeymapError> {
    if direct {
        let direct = overrides
            .into_iter()
            .try_fold(spec.direct, replace_binding)?;
        Ok(KeymapSpec { direct, ..spec })
    } else {
        let prefix = overrides
            .into_iter()
            .try_fold(spec.prefix, replace_binding)?;
        Ok(KeymapSpec { prefix, ..spec })
    }
}

fn replace_binding(
    bindings: Vec<Binding>,
    override_: KeyBindingOverride,
) -> Result<Vec<Binding>, KeymapError> {
    if override_.aliases.is_empty() {
        return Err(KeymapError::EmptyAliases {
            action: override_.action,
        });
    }
    let aliases = override_
        .aliases
        .iter()
        .map(|value| KeyPattern::parse(value))
        .collect::<Result<Vec<_>, _>>()?;
    if !bindings
        .iter()
        .any(|binding| binding.action == override_.action)
    {
        return Err(KeymapError::UnknownAction {
            action: override_.action,
        });
    }
    Ok(bindings
        .into_iter()
        .map(|binding| {
            if binding.action == override_.action {
                Binding {
                    aliases: aliases.clone(),
                    ..binding
                }
            } else {
                binding
            }
        })
        .collect())
}

impl Default for Keymap {
    fn default() -> Self {
        Self::configured(KeymapOverrides::default()).expect("default key bindings must be valid")
    }
}

impl Keymap {
    pub(crate) fn configured(overrides: KeymapOverrides) -> Result<Self, KeymapErrors> {
        let spec = KeymapSpec::default()
            .with_overrides(overrides)
            .map_err(|error| KeymapErrors(vec![error]))?;
        Self::compile(spec)
    }

    fn compile(spec: KeymapSpec) -> Result<Self, KeymapErrors> {
        let errors = validate_spec(&spec);
        if !errors.is_empty() {
            return Err(KeymapErrors(errors));
        }
        Ok(Self {
            leader: spec.leader,
            direct: spec.direct,
            prefix: spec.prefix,
            prefix_active: false,
        })
    }

    pub(crate) fn resolve(&mut self, key: &KeyEvent) -> KeyResolution {
        if self.prefix_active {
            self.prefix_active = false;
            if self.leader.matches(key) {
                return KeyResolution::Action(KeyAction::SendLeaderLiteral);
            }
            return self
                .prefix
                .iter()
                .find_map(|binding| binding.resolve(key))
                .map_or(
                    KeyResolution::Action(KeyAction::CancelPrefix),
                    KeyResolution::Action,
                );
        }

        if self.leader.matches(key) {
            self.prefix_active = true;
            return KeyResolution::Consumed;
        }
        self.direct
            .iter()
            .find_map(|binding| binding.resolve(key))
            .map_or(KeyResolution::PassThrough, KeyResolution::Action)
    }

    pub(crate) fn current_hint(&self) -> KeyHint {
        if self.prefix_active {
            KeyHint {
                text: format!("PREFIX  {}", binding_hints(&self.prefix).join(" · ")),
                prefix_active: true,
            }
        } else {
            KeyHint {
                text: format!("Keybinds: {}", self.leader.label()),
                prefix_active: false,
            }
        }
    }

    pub(crate) fn invocation_hint(&self, action: KeyAction) -> Option<String> {
        let binding = self
            .prefix
            .iter()
            .find(|binding| binding.action == action)?;
        let alias = binding.aliases.first()?.label();
        Some(format!("{} {alias}", self.leader.label()))
    }

    pub(crate) fn invocation_hints(&self, actions: &[KeyAction]) -> Option<String> {
        let aliases = actions
            .iter()
            .map(|action| {
                self.prefix
                    .iter()
                    .find(|binding| binding.action == *action)?
                    .aliases
                    .first()
                    .map(KeyPattern::label)
            })
            .collect::<Option<Vec<_>>>()?;
        Some(format!("{} {}", self.leader.label(), aliases.join("/")))
    }

    pub(crate) fn is_bound(&self, action: KeyAction) -> bool {
        self.prefix.iter().any(|binding| binding.action == action)
    }

    pub(crate) fn literal_leader(&self) -> KeyEvent {
        self.leader
            .literal_event()
            .expect("compiled leaders always identify one key")
    }
}

fn validate_spec(spec: &KeymapSpec) -> Vec<KeymapError> {
    let mut errors = Vec::new();
    validate_context("direct", Some(&spec.leader), &spec.direct, &mut errors);
    // Inside PREFIX the leader intentionally wins and sends itself literally;
    // a plain binding that shares its character (for example Ctrl+B + `b`)
    // remains reachable without the leader's modifiers.
    validate_context("prefix", None, &spec.prefix, &mut errors);
    for binding in &spec.prefix {
        for alias in &binding.aliases {
            if alias.is_fully_shadowed_by(&spec.leader) {
                errors.push(KeymapError::LeaderConflict {
                    context: "prefix",
                    key: alias.label(),
                    action: binding.action,
                });
            }
        }
    }
    errors
}

fn validate_context(
    context: &'static str,
    reserved_leader: Option<&KeyPattern>,
    bindings: &[Binding],
    errors: &mut Vec<KeymapError>,
) {
    for binding in bindings {
        let action = binding.action;
        if binding.aliases.is_empty() {
            errors.push(KeymapError::EmptyAliases { action });
        }
        for (alias_index, alias) in binding.aliases.iter().enumerate() {
            if reserved_leader.is_some_and(|leader| leader.overlaps(alias)) {
                errors.push(KeymapError::LeaderConflict {
                    context,
                    key: alias.label(),
                    action,
                });
            }
            if binding.aliases[alias_index + 1..]
                .iter()
                .any(|other| alias.overlaps(other))
            {
                errors.push(KeymapError::DuplicateAlias {
                    context,
                    key: alias.label(),
                    action,
                });
            }
        }
    }
    for (left_index, left) in bindings.iter().enumerate() {
        for right in &bindings[left_index + 1..] {
            for left_alias in &left.aliases {
                for right_alias in &right.aliases {
                    if left_alias.overlaps(right_alias) {
                        errors.push(KeymapError::ConflictingBinding {
                            context,
                            key: left_alias.label(),
                            first: left.action,
                            second: right.action,
                        });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
