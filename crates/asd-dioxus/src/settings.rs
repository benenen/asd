//! Saved SSH connections and their persistence (`config.json` under the asd
//! data dir). The settings UI itself is plain RSX in [`crate::app`]; this
//! module holds only `Send` data and the form-validation rules.

use serde::{Deserialize, Serialize};

// ── config persistence ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SshConnection {
    /// Stable identity, assigned on creation and persisted. Edit/Delete match on
    /// this, never on the render-time list position (which can shift under a
    /// concurrent add/remove). `0` marks an entry from an older config that
    /// predates the field; [`SettingsConfig::load`] assigns it a real id.
    #[serde(default)]
    pub id: u64,
    pub name: String,
    pub host: String,
    pub user: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// How to authenticate to this host. Defaults to key-based so existing
    /// configs (written before this field existed) keep working.
    #[serde(default)]
    pub auth: SshAuth,
}

fn default_port() -> u16 {
    22
}

/// How a saved connection authenticates. `Password` stores the password inline;
/// `Key` names a private-key file (empty path = try the default `~/.ssh` keys),
/// with an optional passphrase.
///
/// Note: secrets are persisted in the local config file (`config.json`) in
/// plain text — same trust model as `~/.ssh` on a single-user machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum SshAuth {
    Password {
        password: String,
    },
    Key {
        key_path: String,
        passphrase: String,
    },
}

impl Default for SshAuth {
    fn default() -> Self {
        Self::Key {
            key_path: String::new(),
            passphrase: String::new(),
        }
    }
}

impl SshAuth {
    fn kind(&self) -> AuthKind {
        match self {
            Self::Password { .. } => AuthKind::Password,
            Self::Key { .. } => AuthKind::Key,
        }
    }

    /// One-word tag for the connection list ("password" / "key").
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Password { .. } => "password",
            Self::Key { .. } => "key",
        }
    }
}

/// The two authentication choices offered in the form's segmented toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    Password,
    Key,
}

impl SshConnection {
    /// `user@host`, hiding the port when it is the default.
    pub fn label(&self) -> String {
        if self.port == 22 {
            format!("{}@{}", self.user, self.host)
        } else {
            format!("{}@{}:{}", self.user, self.host, self.port)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SettingsConfig {
    #[serde(default)]
    pub ssh_connections: Vec<SshConnection>,
}

impl SettingsConfig {
    pub fn load() -> Self {
        let path = config_path();
        let mut cfg: Self = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        };
        // Backfill stable ids for entries written before the field existed.
        assign_ids(&mut cfg.ssh_connections);
        cfg
    }

    pub fn save(&self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, json);
        }
    }
}

fn config_path() -> std::path::PathBuf {
    asd_proto::paths::data_dir().join("config.json")
}

/// The next free connection id (max + 1, never 0 — `0` marks "unassigned").
pub fn next_id(list: &[SshConnection]) -> u64 {
    list.iter().map(|c| c.id).max().unwrap_or(0) + 1
}

/// Give every id-less (`id == 0`) entry a fresh unique id. Run on load so
/// pre-id configs — and any hand-edited file — get stable identities.
pub fn assign_ids(list: &mut [SshConnection]) {
    let mut next = list.iter().map(|c| c.id).max().unwrap_or(0) + 1;
    for c in list.iter_mut() {
        if c.id == 0 {
            c.id = next;
            next += 1;
        }
    }
}

/// Remove the connection with `id`, returning the new list. Matching on the
/// stable id (not a render-time index) means a concurrent add/remove can't make
/// the click land on the wrong row; an unknown id leaves the list unchanged.
pub fn remove_by_id(list: &[SshConnection], id: u64) -> Vec<SshConnection> {
    list.iter().filter(|c| c.id != id).cloned().collect()
}

/// Insert or update `conn` by id: replace the entry sharing its id, or append it
/// (assigning a fresh id when it has none). Returns the new list — the single
/// save path for both "Add" and "Edit", identity-based so an edit can't
/// overwrite the wrong row.
pub fn upsert(list: &[SshConnection], mut conn: SshConnection) -> Vec<SshConnection> {
    if conn.id != 0
        && let Some(pos) = list.iter().position(|c| c.id == conn.id)
    {
        let mut out = list.to_vec();
        out[pos] = conn;
        return out;
    }
    conn.id = next_id(list);
    let mut out = list.to_vec();
    out.push(conn);
    out
}

// ── UI state ──────────────────────────────────────────────────────────

/// Which settings page is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    General,
    Connections,
}

/// The add/edit connection form. `id` is `Some(existing id)` when editing an
/// existing entry, `None` for a new one (id assigned on save).
#[derive(Debug, Clone, PartialEq)]
pub struct SshForm {
    pub id: Option<u64>,
    pub name: String,
    pub host: String,
    pub user: String,
    pub port: String,
    pub auth_kind: AuthKind,
    pub password: String,
    pub key_path: String,
    pub passphrase: String,
}

impl Default for SshForm {
    fn default() -> Self {
        Self {
            id: None,
            name: String::new(),
            host: String::new(),
            user: String::new(),
            port: String::from("22"),
            auth_kind: AuthKind::Key,
            password: String::new(),
            key_path: String::new(),
            passphrase: String::new(),
        }
    }
}

impl SshForm {
    pub fn from_conn(c: &SshConnection) -> Self {
        let (password, key_path, passphrase) = match &c.auth {
            SshAuth::Password { password } => (password.clone(), String::new(), String::new()),
            SshAuth::Key {
                key_path,
                passphrase,
            } => (String::new(), key_path.clone(), passphrase.clone()),
        };
        Self {
            id: Some(c.id),
            name: c.name.clone(),
            host: c.host.clone(),
            user: c.user.clone(),
            port: c.port.to_string(),
            auth_kind: c.auth.kind(),
            password,
            key_path,
            passphrase,
        }
    }

    /// The first reason the form can't be saved, phrased for the user, or
    /// `None` when it is valid. Drives both the disabled Save button and the
    /// inline hint.
    pub fn invalid_reason(&self) -> Option<&'static str> {
        if self.name.trim().is_empty() {
            return Some("Name is required.");
        }
        if self.host.trim().is_empty() {
            return Some("Host is required.");
        }
        if self.user.trim().is_empty() {
            return Some("User is required.");
        }
        if self.port.trim().parse::<u16>().is_err() {
            return Some("Port must be a number (1–65535).");
        }
        if self.auth_kind == AuthKind::Password && self.password.is_empty() {
            return Some("Password is required.");
        }
        None
    }

    pub fn valid(&self) -> bool {
        self.invalid_reason().is_none()
    }

    fn auth(&self) -> SshAuth {
        match self.auth_kind {
            AuthKind::Password => SshAuth::Password {
                password: self.password.clone(),
            },
            AuthKind::Key => SshAuth::Key {
                key_path: self.key_path.trim().to_string(),
                passphrase: self.passphrase.clone(),
            },
        }
    }

    // Borrows rather than consumes (the form stays editable), despite the name.
    #[allow(clippy::wrong_self_convention)]
    pub fn into_connection(&self) -> Option<SshConnection> {
        if !self.valid() {
            return None;
        }
        Some(SshConnection {
            // `0` when adding — `upsert` assigns a fresh id; the existing id
            // when editing, so the update lands on the right entry.
            id: self.id.unwrap_or(0),
            name: self.name.trim().to_string(),
            host: self.host.trim().to_string(),
            user: self.user.trim().to_string(),
            port: self.port.trim().parse().unwrap_or(22),
            auth: self.auth(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_required() {
        let mut f = SshForm {
            host: "h".into(),
            user: "u".into(),
            ..Default::default()
        };
        assert_eq!(f.invalid_reason(), Some("Name is required."));
        f.name = "dev".into();
        assert!(f.valid());
    }

    #[test]
    fn password_auth_requires_password() {
        let mut f = SshForm {
            name: "dev".into(),
            host: "h".into(),
            user: "u".into(),
            auth_kind: AuthKind::Password,
            ..Default::default()
        };
        assert_eq!(f.invalid_reason(), Some("Password is required."));
        f.password = "s3cret".into();
        let conn = f.into_connection().expect("valid");
        assert_eq!(conn.auth.tag(), "password");
    }

    fn conn(id: u64, n: &str) -> SshConnection {
        SshConnection {
            id,
            name: n.into(),
            host: "h".into(),
            user: "u".into(),
            port: 22,
            auth: SshAuth::default(),
        }
    }

    #[test]
    fn remove_by_id_targets_identity_not_position() {
        let list = vec![conn(1, "a"), conn(2, "b"), conn(3, "d")];
        // Removing id 2 drops "b" wherever it sits, regardless of index.
        let after = remove_by_id(&list, 2);
        assert_eq!(
            after.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["a", "d"]
        );
        // Unknown id → unchanged (no panic, no wrong-row deletion).
        assert_eq!(remove_by_id(&list, 99), list);
    }

    #[test]
    fn upsert_edits_by_id_and_appends_new_with_fresh_id() {
        let list = vec![conn(1, "a"), conn(2, "b")];
        // Edit id 1 → replaces in place, keeps the id, doesn't grow the list.
        let edited = upsert(&list, conn(1, "a2"));
        assert_eq!(edited.len(), 2);
        assert_eq!(edited[0].name, "a2");
        assert_eq!(edited[0].id, 1);
        // Add (id 0) → appended with the next free id (3), never colliding.
        let added = upsert(&list, conn(0, "c"));
        assert_eq!(added.len(), 3);
        assert_eq!(added[2].name, "c");
        assert_eq!(added[2].id, 3);
        // An edit whose id no longer exists is treated as an add (bounds-safe).
        assert_eq!(upsert(&list, conn(9, "x")).len(), 3);
    }

    #[test]
    fn assign_ids_backfills_only_zero_ids_uniquely() {
        let mut list = vec![conn(0, "a"), conn(5, "b"), conn(0, "c")];
        assign_ids(&mut list);
        let ids: Vec<u64> = list.iter().map(|c| c.id).collect();
        assert_eq!(ids[1], 5); // existing id preserved
        assert_ne!(ids[0], 0); // backfilled
        assert_ne!(ids[2], 0);
        assert_ne!(ids[0], ids[2]); // and unique
        assert!(ids[0] > 5 && ids[2] > 5); // above the max existing
    }

    #[test]
    fn old_config_without_auth_defaults_to_key() {
        let json = r#"{"ssh_connections":[{"name":"a","host":"h","user":"u"}]}"#;
        let cfg: SettingsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.ssh_connections[0].auth, SshAuth::default());
        assert_eq!(cfg.ssh_connections[0].port, 22);
    }
}
