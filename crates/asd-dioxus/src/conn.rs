//! Daemon connection: Unix socket + handshake + session management.

use anyhow::Context;
use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, PROTO_VERSION, code};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

pub async fn connect_local() -> anyhow::Result<(FrameReader<BoxRead>, FrameWriter<BoxWrite>)> {
    let socket = asd_proto::paths::socket_path();
    let stream = tokio::net::UnixStream::connect(&socket)
        .await
        .with_context(|| format!("connect {}", socket.display()))?;
    let (r, w) = tokio::io::split(stream);
    Ok((FrameReader::new(Box::new(r)), FrameWriter::new(Box::new(w))))
}

pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

pub async fn handshake(
    writer: &mut FrameWriter<BoxWrite>,
    reader: &mut FrameReader<BoxRead>,
) -> anyhow::Result<()> {
    writer
        .write_frame(&Frame::Hello {
            proto_version: PROTO_VERSION,
            kind: ClientKind::Gui,
        })
        .await?;
    match reader.read_frame().await? {
        Some(Frame::HelloAck { .. }) => Ok(()),
        Some(Frame::Error { code, msg }) => {
            anyhow::bail!("handshake rejected ({code}): {msg}")
        }
        other => anyhow::bail!("unexpected handshake response: {other:?}"),
    }
}

#[derive(Debug)]
pub enum DaemonEvent {
    Sessions(Vec<asd_proto::SessionInfo>),
    Output(Vec<u8>),
    Snapshot(Vec<u8>),
    SessionEnded { name: String, msg: String },
    Created { name: String },
}

pub async fn run(
    ev_tx: mpsc::UnboundedSender<DaemonEvent>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
) -> anyhow::Result<()> {
    let (mut reader, mut writer) = connect_local().await?;
    handshake(&mut writer, &mut reader).await?;
    tracing::info!("connected to daemon");

    let mut attached: Option<String> = None;
    let mut attaching = false;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(1500));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    writer.write_frame(&Frame::ListSessions).await?;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let _ = writer.write_frame(&Frame::ListSessions).await;
            }
            frame = reader.read_frame() => match frame {
                Ok(Some(Frame::SessionList { sessions })) => {
                    let _ = ev_tx.send(DaemonEvent::Sessions(sessions));
                }
                Ok(Some(Frame::Snapshot { vt: dump })) => {
                    tracing::debug!("conn: Snapshot {} bytes", dump.len());
                    attaching = false;
                    let _ = ev_tx.send(DaemonEvent::Snapshot(dump));
                }
                Ok(Some(Frame::Output { bytes })) => {
                    if attaching { continue; }
                    let _ = ev_tx.send(DaemonEvent::Output(bytes));
                }
                Ok(Some(Frame::Created { name })) => {
                    let _ = ev_tx.send(DaemonEvent::Created { name });
                    let _ = writer.write_frame(&Frame::ListSessions).await;
                }
                Ok(Some(Frame::Error { code, msg })) => {
                    if code == code::SESSION_EXITED {
                        if let Some(name) = attached.take() {
                            let _ = ev_tx.send(DaemonEvent::SessionEnded { name, msg });
                        }
                    } else {
                        tracing::debug!(code, %msg, "daemon error");
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => anyhow::bail!("connection closed"),
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::Attach { name, cols, rows }) => {
                    tracing::info!("conn: Attach to {name} {cols}x{rows}");
                    if attached.is_some() {
                        let _ = writer.write_frame(&Frame::Detach).await;
                    }
                    attaching = true;
                    attached = Some(name.clone());
                    writer.write_frame(&Frame::Attach { name, cols, rows }).await?;
                }
                Some(Cmd::Detach) => {
                    if attached.take().is_some() {
                        let _ = writer.write_frame(&Frame::Detach).await;
                    }
                    attaching = false;
                }
                Some(Cmd::Input { bytes }) => {
                    writer.write_frame(&Frame::Input { bytes }).await?;
                }
                Some(Cmd::Resize { cols, rows }) => {
                    writer.write_frame(&Frame::Resize { cols, rows }).await?;
                }
                Some(Cmd::Create) => {
                    tracing::info!("conn: Create");
                    writer.write_frame(&Frame::Create { name: None, cmd: None }).await?;
                }
                Some(Cmd::Kill { name }) => {
                    writer.write_frame(&Frame::Kill { name }).await?;
                    let _ = writer.write_frame(&Frame::ListSessions).await;
                }
                Some(Cmd::Shutdown) | None => {
                    tracing::info!("conn: Shutdown");
                    break;
                }
            },
        }
    }
    if attached.is_some() {
        let _ = writer.write_frame(&Frame::Detach).await;
    }
    Ok(())
}

#[derive(Debug)]
pub enum Cmd {
    Attach { name: String, cols: u16, rows: u16 },
    Detach,
    Input { bytes: Vec<u8> },
    Resize { cols: u16, rows: u16 },
    Create,
    Kill { name: String },
    Shutdown,
}
