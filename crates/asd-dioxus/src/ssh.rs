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
use std::sync::Arc;

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
    let handler = HostKeyVerifier {
        host: spec.host.clone(),
        port: spec.port,
    };
    // russh does the TCP connect + SSH handshake from the address.
    let mut handle = client::connect(config, (spec.host.as_str(), spec.port), handler)
        .await
        .with_context(|| format!("ssh connect {}:{}", spec.host, spec.port))?;

    authenticate(&mut handle, spec)
        .await
        .with_context(|| format!("ssh auth as {}@{}", spec.user, spec.host))?;

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

/// Verifies the server host key against `~/.ssh/known_hosts`, rejecting
/// unknown or changed keys (default russh behavior would reject everything).
struct HostKeyVerifier {
    host: String,
    port: u16,
}

impl client::Handler for HostKeyVerifier {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        match russh::keys::check_known_hosts(&self.host, self.port, server_public_key) {
            Ok(known) => Ok(known),
            // A key that mismatches a recorded one, or an unreadable
            // known_hosts, is a hard reject — do not trust on error.
            Err(e) => {
                tracing::warn!(host = %self.host, error = %e, "known_hosts check failed");
                Ok(false)
            }
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
