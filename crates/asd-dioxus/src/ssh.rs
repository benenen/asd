//! SSH transport for remote hosts, via the pure-Rust `russh` client — no
//! subprocess, so the client stays viable on Windows.
//!
//! We open a session channel and `exec` `asd attach --stdio` on the far end,
//! which transparently proxies the remote daemon's Unix socket to the channel's
//! stdio. The channel's [`tokio::io::AsyncRead`]+[`tokio::io::AsyncWrite`]
//! stream then speaks the normal asd protocol, exactly like a local
//! `UnixStream`.
//!
//! Auth follows the saved connection's [`SshAuth`]: a stored password, an
//! explicit private-key file (with optional passphrase), or — when no key path
//! is set — the unencrypted default `~/.ssh` keys. Host keys are checked against
//! `~/.ssh/known_hosts` and unknown/changed keys are rejected. ssh-agent and
//! 2FA prompts are still a follow-up.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow};
use russh::client;
use russh::keys::{PrivateKeyWithHashAlg, PublicKey, load_secret_key};

use crate::conn::{BoxRead, BoxWrite};
use crate::model::RemoteSpec;
use crate::settings::SshAuth;

/// The command run on the remote: proxy its daemon socket to our channel.
const REMOTE_CMD: &str = "asd attach --stdio";

/// Dial `spec` over SSH and return the boxed protocol stream halves.
pub async fn open(spec: &RemoteSpec) -> anyhow::Result<(BoxRead, BoxWrite)> {
    let config = Arc::new(client::Config::default());
    // The handler records a specific reason (unknown vs changed host key) so a
    // rejection surfaces as an actionable message rather than russh's generic
    // handshake error.
    let host_key_issue = Arc::new(Mutex::new(None::<String>));
    let handler = HostKeyVerifier {
        host: spec.host.clone(),
        port: spec.port,
        trust: false,
        issue: Arc::clone(&host_key_issue),
    };
    // russh does the TCP connect + SSH handshake from the address.
    let mut handle = match client::connect(config, (spec.host.as_str(), spec.port), handler).await {
        Ok(h) => h,
        Err(e) => {
            // A host-key rejection aborts the handshake here — prefer the
            // handler's specific reason over the opaque russh error.
            if let Some(reason) = host_key_issue.lock().unwrap().take() {
                return Err(anyhow!(reason));
            }
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("ssh connect {}:{}", spec.host, spec.port));
        }
    };

    authenticate(&mut handle, spec)
        .await
        .with_context(|| format!("ssh auth as {}@{} rejected", spec.user, spec.host))?;

    let channel = handle
        .channel_open_session()
        .await
        .context("opening ssh session channel")?;
    channel
        .exec(true, REMOTE_CMD)
        .await
        .context("exec `asd attach --stdio` on remote")?;

    let (rr, ww) = tokio::io::split(channel.into_stream());
    Ok((Box::new(rr), Box::new(ww)))
}

/// Verifies the server host key against `~/.ssh/known_hosts`. In normal mode an
/// unknown or changed key is rejected and a specific, actionable reason is
/// recorded in `issue` so the UI can tell "unknown host" (safe to add) from
/// "key CHANGED" (possible MITM). In `trust` mode the offered key is recorded
/// (replacing any stale entry) and accepted — the "Trust host key" action.
struct HostKeyVerifier {
    host: String,
    port: u16,
    trust: bool,
    issue: Arc<Mutex<Option<String>>>,
}

impl client::Handler for HostKeyVerifier {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        let status = russh::keys::check_known_hosts(&self.host, self.port, server_public_key);
        if matches!(status, Ok(true)) {
            return Ok(true); // known and matches
        }
        if self.trust {
            // The user chose to trust: record (or replace) the key and accept.
            match write_known_host(&self.host, self.port, server_public_key) {
                Ok(()) => return Ok(true),
                Err(e) => {
                    *self.issue.lock().unwrap() = Some(format!("couldn't write known_hosts: {e}"));
                    return Ok(false);
                }
            }
        }
        let reason = match status {
            Ok(true) => unreachable!("handled above"),
            Ok(false) => format!(
                "unknown host key for {} — not in ~/.ssh/known_hosts. Verify it, then click \"Trust host key\".",
                self.host
            ),
            // A recorded key that no longer matches: possible MITM.
            Err(russh::keys::Error::KeyChanged { .. }) => format!(
                "host key CHANGED for {} — possible man-in-the-middle. Only click \"Trust host key\" if this change is expected.",
                self.host
            ),
            // Unreadable known_hosts etc.: reject, don't trust on error.
            Err(e) => format!("known_hosts check failed for {}: {e}", self.host),
        };
        tracing::warn!(host = %self.host, %reason, "host key rejected");
        *self.issue.lock().unwrap() = Some(reason);
        Ok(false)
    }
}

/// Record the server's host key in `~/.ssh/known_hosts` (mirroring OpenSSH's
/// line format), dropping any stale plain entry for the host first so a changed
/// key is replaced rather than duplicated. Then `check_known_hosts` accepts it.
fn write_known_host(host: &str, port: u16, key: &PublicKey) -> anyhow::Result<()> {
    let path = known_hosts_file()?;
    let field = host_field(host, port);
    let openssh = key
        .to_openssh()
        .map_err(|e| anyhow!("encode host key: {e}"))?;
    let new_line = format!("{field} {openssh}");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out: String = existing
        .lines()
        .filter(|line| !line_matches_host(line, &field))
        .map(|l| format!("{l}\n"))
        .collect();
    out.push_str(&new_line);
    out.push('\n');
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, out)?;
    Ok(())
}

/// The known_hosts host token: `host`, or `[host]:port` for a non-default port.
fn host_field(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// Whether a known_hosts line's host token matches `field` (skips comments and
/// hashed `|1|` entries, which can't be matched by plain comparison).
fn line_matches_host(line: &str, field: &str) -> bool {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return false;
    }
    match line.split_whitespace().next() {
        Some(hosts) if !hosts.starts_with("|1|") => hosts.split(',').any(|h| h == field),
        _ => false,
    }
}

fn known_hosts_file() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home).join(".ssh").join("known_hosts"))
}

/// Trust the server's current host key: record it in known_hosts, then future
/// connects verify. Re-dials so we capture the key actually offered now; the
/// handshake completing proves it verifies. The channel is dropped — the caller
/// reconnects for the real session.
pub async fn trust_host_key(spec: &RemoteSpec) -> anyhow::Result<()> {
    let config = Arc::new(client::Config::default());
    let issue = Arc::new(Mutex::new(None::<String>));
    let handler = HostKeyVerifier {
        host: spec.host.clone(),
        port: spec.port,
        trust: true,
        issue: Arc::clone(&issue),
    };
    match client::connect(config, (spec.host.as_str(), spec.port), handler).await {
        Ok(_handle) => Ok(()),
        Err(e) => {
            if let Some(reason) = issue.lock().unwrap().take() {
                return Err(anyhow!(reason));
            }
            Err(anyhow::Error::new(e))
                .with_context(|| format!("ssh connect {}:{}", spec.host, spec.port))
        }
    }
}

/// Authenticate according to the connection's [`SshAuth`].
async fn authenticate(
    handle: &mut client::Handle<HostKeyVerifier>,
    spec: &RemoteSpec,
) -> anyhow::Result<()> {
    let user = &spec.user;
    match &spec.auth {
        SshAuth::Password { password } => {
            let res = handle
                .authenticate_password(user.clone(), password.clone())
                .await?;
            if res.success() {
                Ok(())
            } else {
                Err(anyhow!("password authentication rejected"))
            }
        }
        // An explicit key file (optionally passphrase-protected).
        SshAuth::Key {
            key_path,
            passphrase,
        } if !key_path.trim().is_empty() => {
            let pass = (!passphrase.is_empty()).then_some(passphrase.as_str());
            let key = load_secret_key(key_path.trim(), pass)
                .with_context(|| format!("loading private key {}", key_path.trim()))?;
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), None);
            let res = handle.authenticate_publickey(user, key).await?;
            if res.success() {
                Ok(())
            } else {
                Err(anyhow!("key authentication rejected"))
            }
        }
        // No explicit key path: fall back to the default `~/.ssh` keys.
        SshAuth::Key { .. } => authenticate_default_keys(handle, user).await,
    }
}

/// Try each unencrypted default key file in turn (the pre-config behavior).
async fn authenticate_default_keys(
    handle: &mut client::Handle<HostKeyVerifier>,
    user: &str,
) -> anyhow::Result<()> {
    for path in default_key_paths() {
        if !path.exists() {
            continue;
        }
        let key = match load_secret_key(&path, None) {
            Ok(k) => k,
            // Encrypted or unparsable key: skip it.
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "skipping key");
                continue;
            }
        };
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), None);
        if let Ok(res) = handle.authenticate_publickey(user, key).await
            && res.success()
        {
            return Ok(());
        }
    }
    Err(anyhow!(
        "no usable key in ~/.ssh (id_ed25519/id_ecdsa/id_rsa); set a password or key file for this connection, or add a default key"
    ))
}

/// Default private-key locations, in preference order.
fn default_key_paths() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .iter()
        .map(|name| PathBuf::from(&home).join(".ssh").join(name))
        .collect()
}
