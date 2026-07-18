//! Plain UI state for the session sidebar: the set of hosts (local + SSH
//! remotes), each host's session list, and which session is selected. Pure
//! `Send` data owned by the app's signals; the connections live in per-host
//! tokio tasks (see [`crate::conn`]).

use asd_proto::SessionInfo;

use crate::settings::SshAuth;

/// Stable identifier for a host. `0` is always the local daemon.
pub type HostId = u64;

/// The local daemon's fixed id.
pub const LOCAL_ID: HostId = 0;

/// How to reach a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKind {
    /// The local daemon over its Unix socket.
    Local,
    /// A remote daemon reached over SSH (`asd attach --stdio` on the far end).
    Ssh(RemoteSpec),
}

/// Where and as whom to connect over SSH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSpec {
    /// The saved [`crate::settings::SshConnection`] this came from (its stable
    /// id). Identity is by this, not the address, so two saved connections to
    /// the same `user@host:port` with different credentials are both reachable.
    /// `0` for an ad-hoc spec with no saved entry.
    pub conn_id: u64,
    pub user: String,
    pub host: String,
    pub port: u16,
    /// How to authenticate (password / key). Carried through from the saved
    /// [`crate::settings::SshConnection`] so [`crate::ssh`] can use it.
    pub auth: SshAuth,
    /// The saved connection's display name, shown in the sidebar. May be empty
    /// (older hosts), in which case the short host name is shown instead.
    pub name: String,
}

impl RemoteSpec {
    /// `user@host`, hiding the port when it is the default.
    pub fn label(&self) -> String {
        if self.port == 22 {
            format!("{}@{}", self.user, self.host)
        } else {
            format!("{}@{}:{}", self.user, self.host, self.port)
        }
    }

    /// Short host name for the group header: the part before the first dot.
    pub fn short_host(&self) -> &str {
        self.host.split('.').next().unwrap_or(&self.host)
    }
}

/// Connection state of a host, shown as its status dot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostState {
    Connecting,
    Up,
    /// Failed or dropped, with a human-readable reason.
    Down(String),
}

/// One host and its (last known) session list.
#[derive(Debug, Clone, PartialEq)]
pub struct Host {
    pub id: HostId,
    pub kind: HostKind,
    pub state: HostState,
    pub sessions: Vec<SessionInfo>,
}

impl Host {
    pub fn is_remote(&self) -> bool {
        matches!(self.kind, HostKind::Ssh(_))
    }

    /// Group-header label: `local`, the saved connection's name, or (when it has
    /// none) the remote's short host name.
    pub fn label(&self) -> String {
        match &self.kind {
            HostKind::Local => "local".to_string(),
            HostKind::Ssh(spec) if !spec.name.trim().is_empty() => spec.name.clone(),
            HostKind::Ssh(spec) => spec.short_host().to_string(),
        }
    }

    /// Secondary line: the socket path (local) or `user@host` (remote).
    pub fn sublabel(&self) -> String {
        match &self.kind {
            HostKind::Local => asd_proto::paths::socket_path().display().to_string(),
            HostKind::Ssh(spec) => spec.label(),
        }
    }
}

/// The whole sidebar model plus the current selection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Model {
    pub hosts: Vec<Host>,
    /// The session being viewed: `(host, name)`.
    pub active: Option<(HostId, String)>,
    next_id: HostId,
}

impl Model {
    /// A fresh model with just the local host, in the connecting state.
    pub fn with_local() -> Self {
        Self {
            hosts: vec![Host {
                id: LOCAL_ID,
                kind: HostKind::Local,
                state: HostState::Connecting,
                sessions: Vec::new(),
            }],
            active: None,
            next_id: 1,
        }
    }

    pub fn host(&self, id: HostId) -> Option<&Host> {
        self.hosts.iter().find(|h| h.id == id)
    }

    fn host_mut(&mut self, id: HostId) -> Option<&mut Host> {
        self.hosts.iter_mut().find(|h| h.id == id)
    }

    /// Whether the saved connection with this stable id is already added as a
    /// host. Identity is the saved-connection id (not the address), so a second
    /// saved connection to the same box — with different credentials — is not
    /// falsely reported as already added.
    pub fn has_connection(&self, conn_id: u64) -> bool {
        conn_id != 0
            && self
                .hosts
                .iter()
                .any(|h| matches!(&h.kind, HostKind::Ssh(s) if s.conn_id == conn_id))
    }

    /// Add a remote host (or return the existing one for the same saved
    /// connection). Returns the host id.
    pub fn add_remote(&mut self, spec: RemoteSpec) -> HostId {
        // Dedup by the saved connection's identity, not the address: the same
        // saved entry must not spawn two hosts, but two distinct entries to the
        // same address (different creds) each get their own.
        if spec.conn_id != 0
            && let Some(h) = self
                .hosts
                .iter()
                .find(|h| matches!(&h.kind, HostKind::Ssh(s) if s.conn_id == spec.conn_id))
        {
            return h.id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.hosts.push(Host {
            id,
            kind: HostKind::Ssh(spec),
            state: HostState::Connecting,
            sessions: Vec::new(),
        });
        id
    }

    pub fn remove_host(&mut self, id: HostId) {
        if id == LOCAL_ID {
            return; // the local host is permanent
        }
        self.hosts.retain(|h| h.id != id);
        if self.active.as_ref().is_some_and(|(h, _)| *h == id) {
            self.active = None;
        }
    }

    pub fn set_state(&mut self, id: HostId, state: HostState) {
        if let Some(h) = self.host_mut(id) {
            h.state = state;
        }
    }

    /// Replace a host's session list. If the active session vanished (killed or
    /// exited elsewhere), the selection is cleared.
    pub fn set_sessions(&mut self, id: HostId, sessions: Vec<SessionInfo>) {
        if let Some(h) = self.host_mut(id) {
            h.sessions = sessions;
        }
        if let Some((h, name)) = &self.active
            && *h == id
            && self
                .host(id)
                .is_some_and(|h| !h.sessions.iter().any(|s| &s.name == name))
        {
            self.active = None;
        }
    }

    pub fn select(&mut self, host: HostId, name: String) {
        self.active = Some((host, name));
    }

    /// Optimistically rename a session locally so the sidebar (and the active
    /// selection, if it was the renamed one) update immediately; the next list
    /// poll confirms it, or reverts it if the daemon rejected the rename.
    pub fn rename_session(&mut self, host: HostId, old: &str, new: &str) {
        if let Some(h) = self.host_mut(host)
            && let Some(s) = h.sessions.iter_mut().find(|s| s.name == old)
        {
            s.name = new.to_string();
        }
        if self
            .active
            .as_ref()
            .is_some_and(|(h, n)| *h == host && n == old)
        {
            self.active = Some((host, new.to_string()));
        }
    }

    pub fn is_active(&self, host: HostId, name: &str) -> bool {
        self.active
            .as_ref()
            .is_some_and(|(h, n)| *h == host && n == name)
    }

    pub fn total_sessions(&self) -> usize {
        self.hosts.iter().map(|h| h.sessions.len()).sum()
    }
}

/// Compact a session's command for the sidebar: a bare path shows just its
/// basename (a shell reads as `bash`, not `/usr/bin/bash`); anything with
/// arguments is kept whole, and everything is capped so a long command can't
/// blow out the row width.
pub fn short_cmd(cmd: &str) -> String {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return String::new();
    }
    let base = if !cmd.contains(char::is_whitespace) && cmd.contains('/') {
        cmd.rsplit('/').next().unwrap_or(cmd)
    } else {
        cmd
    };
    const MAX: usize = 24;
    if base.chars().count() > MAX {
        let mut s: String = base.chars().take(MAX - 1).collect();
        s.push('…');
        s
    } else {
        base.to_string()
    }
}

/// Compact "time since creation": `just now`, `5m`, `18m`, `2h`, `3d`.
/// `now_ms`/`created_ms` are Unix-epoch milliseconds.
pub fn short_age(created_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(created_ms) / 1000;
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Whether a proposed session `new` name can be submitted, given the sibling
/// session names on that host (`existing`) and the `current` (old) name.
/// `Err(message)` is shown inline under the rename field. Mirrors the daemon's
/// own validation (`asd_proto::paths::is_valid_session_name`) so most rejections
/// are caught before the round-trip.
pub fn validate_rename(new: &str, existing: &[String], current: &str) -> Result<(), String> {
    let new = new.trim();
    if new == current {
        return Ok(()); // no-op rename is harmless
    }
    if new.is_empty() {
        return Err("name can't be empty".into());
    }
    if !asd_proto::paths::is_valid_session_name(new) {
        return Err("letters, digits, '_' or '-' (max 64)".into());
    }
    if existing.iter().any(|n| n == new) {
        return Err(format!("'{new}' already exists"));
    }
    Ok(())
}

/// Whether a host-down reason is a host-key problem (unknown or changed key, or
/// an unreadable known_hosts) — the cases the "Trust host key" action can fix.
/// Matched against the messages [`crate::ssh`] produces on rejection.
pub fn is_host_key_issue(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    r.contains("host key") || r.contains("known_hosts")
}

/// Truncate a host-down reason to fit the sidebar's reason line.
pub fn short_reason(msg: &str) -> String {
    const MAX: usize = 52;
    let msg = msg.trim();
    if msg.chars().count() > MAX {
        let mut s: String = msg.chars().take(MAX - 1).collect();
        s.push('…');
        s
    } else {
        msg.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, created_ms: u64, clients: u32) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            command: "sh".into(),
            title: String::new(),
            created_ms,
            idle_ms: 0,
            running: false,
            attached_clients: clients,
            cols: 80,
            rows: 24,
        }
    }

    fn spec(user: &str, host: &str, port: u16) -> RemoteSpec {
        spec_id(1, user, host, port)
    }

    fn spec_id(conn_id: u64, user: &str, host: &str, port: u16) -> RemoteSpec {
        RemoteSpec {
            conn_id,
            user: user.into(),
            host: host.into(),
            port,
            auth: SshAuth::default(),
            name: String::new(),
        }
    }

    #[test]
    fn short_cmd_basenames_paths_but_keeps_args() {
        assert_eq!(short_cmd("/usr/bin/bash"), "bash");
        assert_eq!(short_cmd("journalctl -f"), "journalctl -f");
        assert_eq!(short_cmd(""), "");
        let long = short_cmd("python train.py --config really/long/path/to/config.yaml");
        assert_eq!(long.chars().count(), 24);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn remote_spec_labels_hide_default_port() {
        let s = spec("deploy", "gpu-01.lab", 22);
        assert_eq!(s.label(), "deploy@gpu-01.lab");
        assert_eq!(s.short_host(), "gpu-01");
        let s = spec("deploy", "edge-7", 2200);
        assert_eq!(s.label(), "deploy@edge-7:2200");
    }

    #[test]
    fn add_remote_dedups_by_connection_id_not_address() {
        let mut m = Model::with_local();
        let id1 = m.add_remote(spec_id(1, "me", "b", 22));
        // The same saved connection (id 1), even with an edited name, dedupes.
        let mut renamed = spec_id(1, "me", "b", 22);
        renamed.name = "renamed".into();
        assert_eq!(m.add_remote(renamed), id1);
        assert_eq!(m.hosts.len(), 2); // local + one remote
        assert!(m.has_connection(1));
        assert!(!m.has_connection(2));
        // A *different* saved connection to the same address (id 2) is its own
        // host — different credentials must both be reachable.
        let id2 = m.add_remote(spec_id(2, "me", "b", 22));
        assert_ne!(id1, id2);
        assert_eq!(m.hosts.len(), 3);
        assert!(m.has_connection(2));
    }

    #[test]
    fn killing_the_active_session_clears_selection() {
        let mut m = Model::with_local();
        m.set_sessions(LOCAL_ID, vec![info("web", 0, 1), info("logs", 0, 0)]);
        m.select(LOCAL_ID, "web".into());
        assert!(m.is_active(LOCAL_ID, "web"));
        m.set_sessions(LOCAL_ID, vec![info("logs", 0, 0)]);
        assert_eq!(m.active, None);
    }

    #[test]
    fn short_age_buckets() {
        let m = 60_000;
        assert_eq!(short_age(0, 30 * 1000), "just now");
        assert_eq!(short_age(0, 5 * m), "5m");
        assert_eq!(short_age(0, 120 * m), "2h");
        assert_eq!(short_age(0, 3 * 24 * 60 * m), "3d");
        assert_eq!(short_age(1_000, 0), "just now");
    }

    #[test]
    fn host_key_issue_detection() {
        assert!(is_host_key_issue(
            "unknown host key for gpu-01 — not in ~/.ssh/known_hosts."
        ));
        assert!(is_host_key_issue(
            "host key CHANGED for gpu-01 — possible MITM"
        ));
        assert!(is_host_key_issue("known_hosts check failed for h: bad"));
        // Auth / network failures are NOT host-key issues (no Trust button).
        assert!(!is_host_key_issue("password authentication rejected"));
        assert!(!is_host_key_issue(
            "ssh connect gpu-01:22: connection refused"
        ));
    }

    #[test]
    fn local_host_is_permanent() {
        let mut m = Model::with_local();
        m.remove_host(LOCAL_ID);
        assert_eq!(m.hosts.len(), 1);
    }

    #[test]
    fn validate_rename_rejects_empty_invalid_dup_but_allows_noop() {
        let existing = vec!["web".to_string(), "logs".to_string()];
        assert!(validate_rename("", &existing, "web").is_err()); // empty
        assert!(validate_rename("a b", &existing, "web").is_err()); // space
        assert!(validate_rename("中文", &existing, "web").is_err()); // non-ascii
        assert!(validate_rename("logs", &existing, "web").is_err()); // duplicate
        assert!(validate_rename("web", &existing, "web").is_ok()); // no-op
        assert!(validate_rename("api", &existing, "web").is_ok()); // fresh + valid
        assert!(validate_rename("  api  ", &existing, "web").is_ok()); // trimmed
    }

    #[test]
    fn rename_session_updates_row_and_active() {
        let mut m = Model::with_local();
        m.set_sessions(LOCAL_ID, vec![info("web", 0, 1), info("logs", 0, 0)]);
        m.select(LOCAL_ID, "web".into());
        m.rename_session(LOCAL_ID, "web", "api");
        assert!(
            m.host(LOCAL_ID)
                .unwrap()
                .sessions
                .iter()
                .any(|s| s.name == "api")
        );
        assert!(
            !m.host(LOCAL_ID)
                .unwrap()
                .sessions
                .iter()
                .any(|s| s.name == "web")
        );
        assert!(m.is_active(LOCAL_ID, "api")); // active followed the rename
        // Renaming a non-active session leaves the selection untouched.
        m.rename_session(LOCAL_ID, "logs", "journal");
        assert!(m.is_active(LOCAL_ID, "api"));
    }
}
