//! Session task editing and read-only daemon-side Git review.

use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, SessionIdentity, SessionTask};
use dioxus::prelude::*;

use crate::model::{HostId, HostKind, Model};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Target {
    pub host: HostId,
    pub kind: HostKind,
    pub identity: SessionIdentity,
    pub name: String,
    pub task: Option<SessionTask>,
}

impl Target {
    fn current(&self, model: &Model) -> bool {
        model.host(self.host).is_some_and(|host| {
            host.kind == self.kind && host.sessions.iter().any(|s| s.identity() == self.identity)
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Review {
    task: Option<SessionTask>,
    directory: String,
    branch: String,
    status: String,
    diff: String,
    truncated: bool,
}

fn review_response(identity: SessionIdentity, frame: Frame) -> Result<Review, String> {
    match frame {
        Frame::SessionReview {
            identity: received,
            task,
            directory,
            branch,
            status,
            diff,
            truncated,
        } if received == identity => Ok(Review {
            task,
            directory,
            branch,
            status,
            diff,
            truncated,
        }),
        Frame::Error { msg, .. } => Err(msg),
        _ => Err("Unexpected review response or session identity changed".into()),
    }
}

async fn request(target: Target, task: Option<Option<SessionTask>>) -> Result<Review, String> {
    tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        let (read, write) = match &target.kind {
            HostKind::Local => crate::platform::connect_local().await,
            HostKind::Ssh(spec) => crate::ssh::open(spec).await,
        }
        .map_err(|e| e.to_string())?;
        exchange(target.identity, read, write, task).await
    })
    .await
    .map_err(|_| "Request timed out; refresh to check the current association".to_string())?
}

async fn exchange(
    identity: SessionIdentity,
    read: crate::conn::BoxRead,
    write: crate::conn::BoxWrite,
    task: Option<Option<SessionTask>>,
) -> Result<Review, String> {
    let mut reader = FrameReader::new(read);
    let mut writer = FrameWriter::new(write);
    asd_client::handshake(&mut writer, &mut reader, ClientKind::Gui).await?;
    let updating = task.is_some();
    if let Some(task) = task {
        writer
            .write_frame(&Frame::SetSessionTask { identity, task })
            .await
            .map_err(|e| e.to_string())?;
        match reader.read_frame().await.map_err(|e| e.to_string())? {
            Some(Frame::Ack) => {}
            Some(Frame::Error { msg, .. }) => return Err(msg),
            _ => return Err("Task update was not acknowledged".into()),
        }
    }
    writer
        .write_frame(&Frame::GetSessionReview { identity })
        .await
        .map_err(|e| e.to_string())?;
    let response = reader
        .read_frame()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Daemon closed the review connection".to_string())?;
    review_response(identity, response).map_err(|error| {
        if updating {
            format!("Association saved; review unavailable: {error}")
        } else {
            error
        }
    })
}

#[component]
pub(crate) fn TaskReview(
    target: Target,
    model: Signal<Model>,
    onclose: EventHandler<()>,
) -> Element {
    let initial = target.task.clone();
    let mut description = use_signal(|| {
        initial
            .as_ref()
            .map(|t| t.description.clone())
            .unwrap_or_default()
    });
    let mut directory = use_signal(|| {
        initial
            .as_ref()
            .map(|t| t.directory.clone())
            .unwrap_or_default()
    });
    let mut result = use_signal(|| None::<Result<Review, String>>);
    let mut pending = use_signal(|| true);
    let initial_target = target.clone();
    use_future(move || {
        let target = initial_target.clone();
        async move {
            let received = crate::app::bg()
                .spawn(request(target.clone(), None))
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            if target.current(&model.read()) {
                if let Ok(review) = &received
                    && directory.read().is_empty()
                {
                    directory.set(review.directory.clone());
                }
                result.set(Some(received));
                pending.set(false);
            }
        }
    });
    let current = target.current(&model.read());
    let busy = *pending.read();
    let update = {
        let target = target.clone();
        move |task: Option<Option<SessionTask>>| {
            if *pending.read() || !target.current(&model.read()) {
                return;
            }
            pending.set(true);
            result.set(None);
            let target = target.clone();
            spawn(async move {
                let received = crate::app::bg()
                    .spawn(request(target.clone(), task))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                if target.current(&model.read()) {
                    if let Ok(review) = &received {
                        description.set(
                            review
                                .task
                                .as_ref()
                                .map(|task| task.description.clone())
                                .unwrap_or_default(),
                        );
                        directory.set(
                            review
                                .task
                                .as_ref()
                                .map(|task| task.directory.clone())
                                .unwrap_or_else(|| review.directory.clone()),
                        );
                    }
                    result.set(Some(received));
                    pending.set(false);
                }
            });
        }
    };
    let mut save = update.clone();
    let mut clear = update.clone();
    let mut refresh = update;
    let review = result.read().clone();
    rsx! {
        div { class: "confirm-overlay", role: "dialog", aria_modal: "true", aria_label: "Session task and changes",
            div { class: "task-review",
                div { class: "task-review-head",
                    h2 { "Task and changes · {target.name}" }
                    button { class: "bar-btn", onclick: move |_| onclose.call(()), "Close" }
                }
                if !current {
                    p { role: "alert", "This session or host is no longer available. Close and reopen the review." }
                } else {
                    label { r#for: "task-description", "Task description" }
                    textarea { id: "task-description", value: "{description}", disabled: busy,
                        oninput: move |event| description.set(event.value()) }
                    label { r#for: "task-directory", "Repository / worktree directory on this host" }
                    input { id: "task-directory", value: "{directory}", disabled: busy,
                        oninput: move |event| directory.set(event.value()) }
                    div { class: "task-review-actions",
                        button { class: "bar-btn", disabled: busy || directory.read().trim().is_empty() || description.read().trim().is_empty(),
                            onclick: move |_| save(Some(Some(SessionTask {
                                description: description.read().to_string(), directory: directory.read().to_string(),
                            }))), "Save association" }
                        button { class: "bar-btn", disabled: busy,
                            onclick: move |_| clear(Some(None)), "Clear association" }
                        button { class: "bar-btn", disabled: busy, onclick: move |_| refresh(None), "Refresh changes" }
                    }
                    p { class: "dim", "Changes include the current working tree, not only edits made by this agent. This view is read-only. Review submodule working-file changes separately in the submodule directory." }
                    if busy { p { role: "status", "Loading…" } }
                    if let Some(outcome) = review {
                        match outcome {
                            Err(message) => rsx! { p { role: "alert", "{message}" } },
                            Ok(review) => rsx! {
                                p { "Directory: {review.directory}" }
                                p { "Branch: {review.branch}" }
                                if review.truncated { p { role: "status", "Output truncated. Inspect the repository for the full changes." } }
                                h3 { "Status" }
                                pre { class: "review-output", if review.status.is_empty() { "Working tree clean" } else { "{review.status}" } }
                                h3 { "Diff" }
                                pre { class: "review-output", if review.diff.is_empty() { "No tracked changes" } else { "{review.diff}" } }
                            },
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_host_and_replaced_session_cannot_receive_review() {
        let session = asd_proto::SessionInfo {
            name: "agent".into(),
            instance_id: 7,
            command: "sh".into(),
            title: String::new(),
            status_line: String::new(),
            created_ms: 0,
            idle_ms: 0,
            running: false,
            state: asd_proto::AgentState::Unknown,
            attached_clients: 0,
            pid: 0,
            cols: 80,
            rows: 24,
            task: None,
        };
        let target = Target {
            host: 0,
            kind: HostKind::Local,
            identity: session.identity(),
            name: session.name.clone(),
            task: None,
        };
        let mut model = Model::with_local();
        model.set_sessions(0, vec![session.clone()]);
        assert!(target.current(&model));
        assert!(
            !Target {
                host: 9,
                ..target.clone()
            }
            .current(&model)
        );
        model.set_sessions(
            0,
            vec![asd_proto::SessionInfo {
                instance_id: 8,
                ..session
            }],
        );
        assert!(!target.current(&model));
        model.hosts.clear();
        assert!(!target.current(&model));
    }

    #[tokio::test]
    async fn update_waits_for_ack_and_reviews_exact_identity() {
        let identity = SessionIdentity { instance_id: 7 };
        let task = SessionTask {
            description: "Fix task".into(),
            directory: "/repo/worktree".into(),
        };
        let (client, server) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(client);
        let request = tokio::spawn(exchange(
            identity,
            Box::new(read),
            Box::new(write),
            Some(Some(task.clone())),
        ));
        let (read, write) = tokio::io::split(server);
        let mut reader = FrameReader::new(read);
        let mut writer = FrameWriter::new(write);
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
        assert!(
            matches!(reader.read_frame().await.unwrap(), Some(Frame::SetSessionTask { identity: found, task: Some(found_task) }) if found == identity && found_task == task)
        );
        writer.write_frame(&Frame::Ack).await.unwrap();
        assert!(
            matches!(reader.read_frame().await.unwrap(), Some(Frame::GetSessionReview { identity: found }) if found == identity)
        );
        writer
            .write_frame(&Frame::SessionReview {
                identity,
                task: Some(task.clone()),
                directory: "/repo/worktree".into(),
                branch: "fix".into(),
                status: " M file".into(),
                diff: "+new".into(),
                truncated: false,
            })
            .await
            .unwrap();
        let review = request.await.unwrap().unwrap();
        assert_eq!(review.task, Some(task));
        assert_eq!(review.branch, "fix");
        assert_eq!(review.diff, "+new");
    }

    #[tokio::test]
    async fn failed_task_update_does_not_request_review() {
        let identity = SessionIdentity { instance_id: 8 };
        let (client, server) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(client);
        let request = tokio::spawn(exchange(
            identity,
            Box::new(read),
            Box::new(write),
            Some(None),
        ));
        let (read, write) = tokio::io::split(server);
        let mut reader = FrameReader::new(read);
        let mut writer = FrameWriter::new(write);
        reader.read_frame().await.unwrap();
        writer
            .write_frame(&Frame::HelloAck {
                proto_version: asd_proto::PROTO_VERSION,
                daemon_version: "test".into(),
            })
            .await
            .unwrap();
        assert!(matches!(
            reader.read_frame().await.unwrap(),
            Some(Frame::SetSessionTask { task: None, .. })
        ));
        writer
            .write_frame(&Frame::Error {
                code: 1,
                msg: "Session replaced".into(),
            })
            .await
            .unwrap();
        assert_eq!(request.await.unwrap().unwrap_err(), "Session replaced");
        assert!(reader.read_frame().await.unwrap().is_none());
    }

    #[test]
    fn review_rejects_other_session_and_daemon_error() {
        let identity = SessionIdentity { instance_id: 42 };
        assert!(review_response(identity, Frame::Ack).is_err());
        assert_eq!(
            review_response(
                identity,
                Frame::Error {
                    code: 1,
                    msg: "missing session".into()
                }
            )
            .unwrap_err(),
            "missing session"
        );
        let response = |identity| Frame::SessionReview {
            identity,
            task: None,
            directory: "/repo".into(),
            branch: "feature".into(),
            status: " M src/lib.rs".into(),
            diff: "+change".into(),
            truncated: true,
        };
        assert!(review_response(identity, response(SessionIdentity { instance_id: 43 })).is_err());
        let received = review_response(identity, response(identity)).unwrap();
        assert_eq!(received.directory, "/repo");
        assert_eq!(received.branch, "feature");
        assert_eq!(received.diff, "+change");
        assert!(received.truncated);
    }
}
