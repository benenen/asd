//! Idle/state waits follow one exact identity over cursor-resumable connections.
use crate::{client, exit};
use asd_client::events::EventFeed;
use asd_proto::{AgentState, ClientKind, Frame, IDLE_SETTLE_MS, SessionIdentity, code};
use std::path::Path;
use std::time::Duration;

pub(super) async fn wait(
    socket: &Path,
    initial: client::Client,
    name: &str,
    until: Option<AgentState>,
) -> anyhow::Result<()> {
    let mut feed = EventFeed::default();
    let mut identity: Option<SessionIdentity> = None;
    let mut connection = Some(initial);
    let mut force_reset = false;
    loop {
        let mut c = match connection.take() {
            Some(c) => c,
            None => match client::connect(socket, ClientKind::Cli).await {
                Ok(c) => c,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        let after = if force_reset {
            None
        } else {
            feed.last_cursor()
        };
        if c.writer
            .write_frame(&Frame::SubscribeEvents {
                after,
                wants_notifications: false,
            })
            .await
            .is_err()
        {
            continue;
        }
        let mut started = false;
        loop {
            let frame = match c.reader.read_frame().await {
                Ok(Some(Frame::Error { code, msg })) => {
                    return Err(exit::daemon("wait", code, &msg));
                }
                Ok(Some(frame)) => frame,
                _ => break,
            };
            let accepted = if started {
                feed.apply(frame)
            } else {
                feed.start(frame)
            };
            if accepted.is_err() {
                force_reset = true;
                break;
            }
            started = true;
            force_reset = false;
            let sessions = feed.sessions();
            if identity.is_none() {
                identity = sessions
                    .iter()
                    .find(|s| s.name == name)
                    .map(|s| s.identity());
                if identity.is_none() {
                    return Err(exit::daemon(
                        "wait",
                        code::NO_SUCH_SESSION,
                        &format!("no such session '{name}'"),
                    ));
                }
            }
            let Some(session) = sessions.iter().find(|s| Some(s.identity()) == identity) else {
                return Err(exit::daemon(
                    "wait",
                    code::SESSION_EXITED,
                    &format!("session '{name}' exited"),
                ));
            };
            if until.map_or(session.idle_ms >= IDLE_SETTLE_MS, |want| {
                session.state == want
            }) {
                return Ok(());
            }
        }
        // Backoff only follows a lost/invalid connection, never a healthy feed.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
