//! Dedicated event transport with replay after loss and reset after corruption.

use crate::events::{EventFeed, EventFeedChange};
use asd_proto::{ClientKind, EventCursor, Frame, FrameReader, FrameWriter};
use std::future::Future;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

/// Own one feed for the whole event-connection lifetime. Dropping this future
/// closes the transport, including its server-side notification lease.
pub async fn watch<R, W, F, C, E>(
    mut connect: C,
    kind: ClientKind,
    mut emit: E,
) -> Result<(), String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: Future<Output = Result<(R, W), String>>,
    C: FnMut() -> F,
    E: FnMut(EventCursor, EventFeedChange),
{
    let mut feed = EventFeed::default();
    let mut force_reset = false;
    let mut reconnect = false;
    loop {
        if reconnect {
            if let Some(cursor) = feed.last_cursor() {
                emit(cursor, EventFeedChange::NotificationLease(false));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let opened = async {
            let (r, w) = connect().await?;
            let mut reader = FrameReader::new(r);
            let mut writer = FrameWriter::new(w);
            crate::handshake(&mut writer, &mut reader, kind).await?;
            writer
                .write_frame(&Frame::SubscribeEvents {
                    after: if force_reset {
                        None
                    } else {
                        feed.last_cursor()
                    },
                    wants_notifications: true,
                })
                .await
                .map_err(|e| e.to_string())?;
            let first = reader
                .read_frame()
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "event stream closed before start".to_string())?;
            Ok::<_, String>((reader, writer, first))
        };
        let (mut reader, _writer, first) = tokio::time::timeout(Duration::from_secs(15), opened)
            .await
            .map_err(|_| "event connection timed out".to_string())??;
        match feed.start(first) {
            Ok(change) => {
                force_reset = false;
                emit(feed.last_cursor().expect("accepted start"), change);
            }
            Err(_) => {
                force_reset = true;
                reconnect = true;
                continue;
            }
        }
        while let Ok(Some(frame)) = reader.read_frame().await {
            match feed.apply(frame) {
                Ok(change) => emit(feed.last_cursor().expect("accepted change"), change),
                Err(_) => {
                    force_reset = true;
                    break;
                }
            }
        }
        reconnect = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[tokio::test]
    async fn reconnect_replays_cursor_and_invalid_sequence_requests_reset() {
        let mut clients = VecDeque::new();
        let mut servers = Vec::new();
        for _ in 0..3 {
            let (client, server) = tokio::io::duplex(8192);
            clients.push_back(client);
            servers.push(server);
        }
        let mut accepted = Vec::new();
        let watcher = watch(
            || {
                std::future::ready(
                    clients
                        .pop_front()
                        .map(tokio::io::split)
                        .ok_or_else(|| "finished".into()),
                )
            },
            ClientKind::Gui,
            |cursor, change| accepted.push((cursor, change)),
        );
        let server = async move {
            let mut afters = Vec::new();
            for (index, stream) in servers.into_iter().enumerate() {
                let (r, w) = tokio::io::split(stream);
                let mut reader = FrameReader::new(r);
                let mut writer = FrameWriter::new(w);
                assert!(matches!(
                    reader.read_frame().await.unwrap(),
                    Some(Frame::Hello {
                        kind: ClientKind::Gui,
                        ..
                    })
                ));
                writer
                    .write_frame(&Frame::HelloAck {
                        proto_version: asd_proto::PROTO_VERSION,
                        daemon_version: "test".into(),
                    })
                    .await
                    .unwrap();
                let Some(Frame::SubscribeEvents {
                    after,
                    wants_notifications: true,
                }) = reader.read_frame().await.unwrap()
                else {
                    panic!("dedicated subscription required")
                };
                afters.push(after);
                let cursor = EventCursor {
                    daemon_epoch: [1; 16],
                    sequence: if index == 2 { 9 } else { 0 },
                };
                writer
                    .write_frame(&Frame::EventStreamStarted {
                        cursor,
                        sessions: vec![],
                        reset: index != 1,
                        notification_lease: true,
                    })
                    .await
                    .unwrap();
                if index == 1 {
                    writer
                        .write_frame(&Frame::SessionEvent {
                            cursor: EventCursor {
                                sequence: 3,
                                ..cursor
                            },
                            event: asd_proto::SessionEvent::Exited {
                                identity: asd_proto::SessionIdentity { instance_id: 1 },
                                last_name: "gone".into(),
                                exit: asd_proto::SessionExit {
                                    code: 0,
                                    signal: None,
                                },
                            },
                        })
                        .await
                        .unwrap();
                }
            }
            afters
        };
        let (result, afters) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(watcher, server)
        })
        .await
        .unwrap();
        assert_eq!(result, Err("finished".into()));
        assert_eq!(
            afters,
            [
                None,
                Some(EventCursor {
                    daemon_epoch: [1; 16],
                    sequence: 0
                }),
                None
            ]
        );
        assert_eq!(
            accepted
                .iter()
                .filter(|(_, c)| matches!(c, EventFeedChange::Reset { .. }))
                .count(),
            2
        );
        assert_eq!(
            accepted
                .iter()
                .filter(|(_, c)| matches!(c, EventFeedChange::NotificationLease(true)))
                .count(),
            1
        );
        assert_eq!(accepted.last().unwrap().0.sequence, 9);
    }
}
