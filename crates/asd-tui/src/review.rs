//! Read-only task and Git snapshot from the daemon that owns the session.

use asd_proto::{ClientKind, Frame, FrameReader, FrameWriter, SessionIdentity};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::conn::Cmd;
use crate::{App, CtKey, KeyCode, MouseEvent, MouseEventKind};

pub(crate) struct Review {
    identity: SessionIdentity,
    request: u64,
    text: String,
    scroll: usize,
    horizontal: u16,
}

impl Review {
    pub(crate) fn draw(&self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        let visible = self
            .text
            .lines()
            .skip(self.scroll)
            .take(area.height.saturating_sub(2) as usize)
            .collect::<Vec<_>>()
            .join("\n");
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(visible)
                .style(Style::new().fg(Color::White).bg(Color::Black))
                .scroll((0, self.horizontal))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Task / changes (snapshot) ")
                        .title_bottom(" Esc/q close · r refresh · arrows/PgUp/PgDn scroll "),
                ),
            area,
        );
    }

    fn last_line(&self) -> usize {
        self.text.lines().count().saturating_sub(1)
    }

    fn scroll_by(&mut self, amount: i32) {
        self.scroll = self
            .scroll
            .saturating_add_signed(amount as isize)
            .min(self.last_line());
    }
}

impl App {
    pub(crate) fn toggle_review(&mut self) {
        if self.review.take().is_some() {
            self.dirty = true;
            return;
        }
        let Some(session) = self
            .sessions
            .iter()
            .find(|s| Some(&s.name) == self.active.as_ref())
        else {
            self.notice = Some("Select a session to review".into());
            return;
        };
        self.review_request = self.review_request.wrapping_add(1);
        let identity = session.identity();
        let request = self.review_request;
        self.review = Some(Review {
            identity,
            request,
            text: "Loading task and changes…".into(),
            scroll: 0,
            horizontal: 0,
        });
        self.send(Cmd::Review { identity, request });
        self.dirty = true;
    }

    pub(crate) fn reconcile_review(&mut self) {
        if self.review.as_ref().is_some_and(|review| {
            !self.sessions.iter().any(|session| {
                Some(&session.name) == self.active.as_ref() && session.identity() == review.identity
            })
        }) {
            self.review = None;
        }
    }

    pub(crate) fn receive_review(
        &mut self,
        identity: SessionIdentity,
        request: u64,
        result: Result<Frame, String>,
    ) {
        let valid = self
            .sessions
            .iter()
            .any(|s| Some(&s.name) == self.active.as_ref() && s.identity() == identity);
        if !valid {
            if self
                .review
                .as_ref()
                .is_some_and(|review| review.identity == identity)
            {
                self.review = None;
            }
            return;
        }
        let Some(review) = self
            .review
            .as_mut()
            .filter(|r| r.identity == identity && r.request == request)
        else {
            return;
        };
        review.text = match result {
            Ok(Frame::SessionReview {
                identity: reply_identity,
                task,
                directory,
                branch,
                status,
                diff,
                truncated,
            }) if reply_identity == identity => {
                let description = task
                    .as_ref()
                    .map_or("No task linked", |task| task.description.as_str());
                format!(
                    "Task: {description}\nWorktree: {directory}\nBranch: {branch}\n\nSubmodule working-file changes: review the submodule directory separately.\n\nChanged files (includes untracked):\n{status}\nDiff (staged / unstaged; untracked content excluded):\n{diff}{}",
                    if truncated {
                        "\n[Output truncated]"
                    } else {
                        ""
                    }
                )
            }
            Ok(_) => "Invalid review response from daemon".into(),
            Err(error) => format!("Cannot load review: {error}"),
        };
        // Daemon-supplied task descriptions and Git paths must stay plain text.
        review.text = review
            .text
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
            .collect();
        self.dirty = true;
    }

    pub(crate) fn on_review_key(&mut self, key: CtKey) -> bool {
        let Some(review) = self.review.as_mut() else {
            return false;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.review = None,
            KeyCode::Char('r') => {
                self.review = None;
                self.toggle_review();
            }
            KeyCode::Up | KeyCode::Char('k') => review.scroll_by(-1),
            KeyCode::Down | KeyCode::Char('j') => review.scroll_by(1),
            KeyCode::PageUp => review.scroll_by(-(self.term_size.1.saturating_sub(3) as i32)),
            KeyCode::PageDown => review.scroll_by(self.term_size.1.saturating_sub(3) as i32),
            KeyCode::Home => {
                review.scroll = 0;
                review.horizontal = 0;
            }
            KeyCode::End => review.scroll = review.last_line(),
            KeyCode::Left => review.horizontal = review.horizontal.saturating_sub(4),
            KeyCode::Right => review.horizontal = review.horizontal.saturating_add(4),
            _ => {}
        }
        self.dirty = true;
        true
    }

    pub(crate) fn on_review_mouse(&mut self, mouse: MouseEvent) -> bool {
        let Some(review) = self.review.as_mut() else {
            return false;
        };
        match mouse.kind {
            MouseEventKind::ScrollUp => review.scroll_by(-3),
            MouseEventKind::ScrollDown => review.scroll_by(3),
            _ => {}
        }
        self.dirty = true;
        true
    }
}

pub(crate) async fn fetch(
    socket: &std::path::Path,
    identity: SessionIdentity,
) -> Result<Frame, String> {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let (read, write) = crate::platform::connect_stream(socket).await?;
        let mut reader = FrameReader::new(read);
        let mut writer = FrameWriter::new(write);
        asd_client::handshake(&mut writer, &mut reader, ClientKind::Tui).await?;
        writer
            .write_frame(&Frame::GetSessionReview { identity })
            .await
            .map_err(|e| e.to_string())?;
        match reader.read_frame().await.map_err(|e| e.to_string())? {
            Some(Frame::Error { msg, .. }) => Err(msg),
            Some(frame) => Ok(frame),
            None => Err("daemon closed review connection".into()),
        }
    })
    .await
    .map_err(|_| "review timed out".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_review_scroll_can_reach_tail_beyond_u16_lines() {
        let mut review = Review {
            identity: SessionIdentity { instance_id: 7 },
            request: 1,
            text: format!("{}TAIL", "+x\n".repeat(90_000)),
            scroll: 0,
            horizontal: 0,
        };
        review.scroll_by(90_000);
        assert_eq!(review.scroll, 90_000);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 10)).unwrap();
        terminal.draw(|frame| review.draw(frame)).unwrap();
        let output: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(output.contains("TAIL"));
    }

    #[test]
    fn end_key_reaches_actual_last_line_in_one_press() {
        let (mut app, _) = crate::graph_overlay::tests::app_watching_commands();
        app.review = Some(Review {
            identity: SessionIdentity { instance_id: 7 },
            request: 1,
            text: format!("{}TAIL", "+x\n".repeat(90_000)),
            scroll: 0,
            horizontal: 0,
        });
        app.on_key(CtKey::new(KeyCode::End, crate::KeyModifiers::NONE));
        let review = app.review.as_ref().unwrap();
        assert_eq!(review.scroll, 90_000);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 10)).unwrap();
        terminal.draw(|frame| review.draw(frame)).unwrap();
        let output: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(output.contains("TAIL"));
    }

    #[test]
    fn review_renders_read_only_snapshot_and_scrolls_to_the_end() {
        let text = "Task: fix login\nWorktree: /work/auth\nBranch: fix/auth\n\nChanged files:\n M auth.rs\nDiff:\n-old\n+new";
        let mut review = Review {
            identity: SessionIdentity { instance_id: 7 },
            request: 1,
            text: text.into(),
            scroll: 0,
            horizontal: 0,
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 16)).unwrap();
        terminal.draw(|frame| review.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let output: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        for expected in [
            "fix login",
            "/work/auth",
            "fix/auth",
            " M auth.rs",
            "+new",
            "Esc/q close",
        ] {
            assert!(output.contains(expected), "missing {expected}");
        }
        review.scroll_by(100);
        assert_eq!(review.scroll, 8);
        review.scroll_by(-100);
        assert_eq!(review.scroll, 0);
    }
}

#[cfg(test)]
mod interaction_tests {
    use crate::conn::{Cmd, Ev};
    use crate::graph_overlay::tests::{app_watching_commands, session};
    use crate::{CtKey, KeyCode, KeyModifiers};

    #[test]
    fn review_keeps_input_and_paste_out_of_the_pty_and_ignores_late_reply() {
        let (mut app, mut commands) = app_watching_commands();
        app.sessions = vec![session("work", 42)];
        app.active = Some("work".into());
        app.on_key(CtKey::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.on_key(CtKey::new(KeyCode::Char('v'), KeyModifiers::NONE));
        let Cmd::Review { identity, request } = commands.try_recv().unwrap() else {
            panic!("review must request the daemon's Git snapshot");
        };
        app.on_key(CtKey::new(KeyCode::Char('x'), KeyModifiers::NONE));
        app.on_paste("dangerous shell command");
        assert!(commands.try_recv().is_err());
        app.on_key(CtKey::new(KeyCode::Esc, KeyModifiers::NONE));
        app.on_conn_event(Ev::Review {
            identity,
            request,
            result: Err("late error".into()),
        });
        assert!(app.review.is_none());
    }

    #[test]
    fn review_ignores_reply_for_recreated_session() {
        let (mut app, mut commands) = app_watching_commands();
        app.sessions = vec![session("work", 42)];
        app.active = Some("work".into());
        app.toggle_review();
        let Cmd::Review { identity, request } = commands.try_recv().unwrap() else {
            panic!()
        };
        app.sessions[0].instance_id += 1;
        app.on_conn_event(Ev::Review {
            identity,
            request,
            result: Err("stale error".into()),
        });
        assert!(app.review.is_none());
    }
    #[test]
    fn review_closes_when_session_list_removes_its_identity() {
        let (mut app, _) = app_watching_commands();
        app.sessions = vec![session("work", 42)];
        app.active = Some("work".into());
        app.toggle_review();
        app.on_conn_event(Ev::Sessions(Vec::new()));
        assert!(app.review.is_none());
    }
    #[test]
    fn reopening_review_rejects_previous_request_and_shows_daemon_task_snapshot() {
        let (mut app, mut commands) = app_watching_commands();
        app.sessions = vec![session("work", 42)];
        app.active = Some("work".into());
        app.toggle_review();
        let Cmd::Review {
            identity,
            request: old,
        } = commands.try_recv().unwrap()
        else {
            panic!()
        };
        app.on_key(CtKey::new(KeyCode::Char('r'), KeyModifiers::NONE));
        let Cmd::Review { request, .. } = commands.try_recv().unwrap() else {
            panic!()
        };
        app.on_conn_event(Ev::Review {
            identity,
            request,
            result: Ok(Box::new(asd_proto::Frame::SessionReview {
                identity,
                task: Some(asd_proto::SessionTask {
                    description: "Fix login".into(),
                    directory: "/remote/auth".into(),
                }),
                directory: "/remote/auth".into(),
                branch: "fix/login".into(),
                status: " M login.rs".into(),
                diff: "+new login".into(),
                truncated: true,
            })),
        });
        app.on_conn_event(Ev::Review {
            identity,
            request: old,
            result: Err("stale error".into()),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 20)).unwrap();
        terminal
            .draw(|frame| app.review.as_ref().unwrap().draw(frame))
            .unwrap();
        let output: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        for expected in [
            "Fix login",
            "/remote/auth",
            "fix/login",
            " M login.rs",
            "+new login",
            "Output truncated",
        ] {
            assert!(output.contains(expected), "missing {expected}");
        }
        assert!(!output.contains("stale error"));
    }
}
