//! Read-only workspace navigation through the session's owning daemon.

use asd_proto::{
    ClientKind, Frame, FrameReader, FrameWriter, SessionIdentity, WorkspaceEntry,
    WorkspaceEntryKind,
};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::conn::Cmd;
use crate::{App, CtKey, KeyCode, MouseEvent, MouseEventKind};

pub(crate) struct Files {
    identity: SessionIdentity,
    request: u64,
    root: Option<String>,
    path: String,
    entries: Vec<WorkspaceEntry>,
    selected: usize,
    message: String,
    loading: bool,
}

fn plain(text: &str) -> String {
    text.chars().flat_map(|c| c.escape_debug()).collect()
}

impl Files {
    pub(crate) fn draw(&self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        let root = self.root.as_deref().unwrap_or("Loading workspace…");
        let directory = if self.path.is_empty() {
            root.to_string()
        } else {
            format!("{}/{path}", root.trim_end_matches('/'), path = self.path)
        };
        let rows = area.height.saturating_sub(5).max(1) as usize;
        let start = self.selected.saturating_sub(rows - 1);
        let mut lines = vec![plain(&directory), self.message.clone(), String::new()];
        for (index, entry) in self.entries.iter().enumerate().skip(start).take(rows) {
            let kind = match entry.kind {
                WorkspaceEntryKind::Directory => "dir",
                WorkspaceEntryKind::File => "file",
                WorkspaceEntryKind::Symlink => "link",
                WorkspaceEntryKind::Other => "other",
            };
            lines.push(format!(
                "{} [{kind:5}] {}",
                if index == self.selected { ">" } else { " " },
                plain(&entry.name)
            ));
        }
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(lines.join("\n"))
                .style(Style::new().fg(Color::White).bg(Color::Black))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Workspace files (read-only) ")
                        .title_bottom(
                            " Enter directory · Backspace parent · r refresh · Esc/q close ",
                        ),
                ),
            area,
        );
    }

    fn move_by(&mut self, amount: isize) {
        self.selected = self
            .selected
            .saturating_add_signed(amount)
            .min(self.entries.len().saturating_sub(1));
    }
}

impl App {
    pub(crate) fn close_files(&mut self) {
        if self.files.take().is_some() {
            self.send(Cmd::CancelFiles);
            self.dirty = true;
        }
    }

    pub(crate) fn toggle_files(&mut self) {
        if self.files.is_some() {
            self.close_files();
            return;
        }
        let Some(session) = self
            .sessions
            .iter()
            .find(|s| Some(&s.name) == self.active.as_ref())
        else {
            self.notice = Some("Select a session to browse files".into());
            return;
        };
        let identity = session.identity();
        self.git_graph = None;
        self.review = None;
        self.modal = None;
        self.files = Some(Files {
            identity,
            request: 0,
            root: None,
            path: String::new(),
            entries: Vec::new(),
            selected: 0,
            message: String::new(),
            loading: false,
        });
        self.request_files(String::new());
    }

    fn request_files(&mut self, path: String) {
        let Some(files) = self.files.as_mut() else {
            return;
        };
        self.files_request = self.files_request.wrapping_add(1);
        files.request = self.files_request;
        files.path = path.clone();
        files.loading = true;
        files.entries.clear();
        files.selected = 0;
        files.message = "Loading…".into();
        let command = Cmd::Files {
            identity: files.identity,
            request: files.request,
            path,
        };
        self.send(command);
        self.dirty = true;
    }

    pub(crate) fn reconcile_files(&mut self) {
        if self.files.as_ref().is_some_and(|files| {
            !self
                .sessions
                .iter()
                .any(|s| Some(&s.name) == self.active.as_ref() && s.identity() == files.identity)
        }) {
            self.close_files();
        }
    }

    pub(crate) fn receive_files(
        &mut self,
        identity: SessionIdentity,
        request: u64,
        result: Result<Frame, String>,
    ) {
        self.reconcile_files();
        let Some(files) = self
            .files
            .as_mut()
            .filter(|f| f.identity == identity && f.request == request)
        else {
            return;
        };
        files.loading = false;
        match result {
            Ok(Frame::WorkspaceFiles {
                identity: reply_identity,
                root,
                path,
                mut entries,
                truncated,
            }) if reply_identity == identity && path == files.path => {
                if files.root.as_ref().is_some_and(|old| old != &root) {
                    files.entries.clear();
                    files.root = Some(root);
                    files.path.clear();
                    files.message = "Workspace changed. Press r to load its root.".into();
                } else {
                    files.root = Some(root);
                    entries.sort_by(|a, b| {
                        (a.kind != WorkspaceEntryKind::Directory, &a.name)
                            .cmp(&(b.kind != WorkspaceEntryKind::Directory, &b.name))
                    });
                    files.message = if truncated {
                        "Listing truncated"
                    } else if entries.is_empty() {
                        "Empty directory"
                    } else {
                        "Directories only can be entered; files and links are read-only."
                    }
                    .into();
                    files.entries = entries;
                }
            }
            Ok(_) => files.message = "Invalid file-list response from daemon".into(),
            Err(error) => files.message = format!("Cannot list directory: {}", plain(&error)),
        }
        self.dirty = true;
    }

    pub(crate) fn on_files_key(&mut self, key: CtKey) -> bool {
        let Some(files) = self.files.as_mut() else {
            return false;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close_files(),
            KeyCode::Char('r') => {
                let path = files.path.clone();
                self.request_files(path);
            }
            KeyCode::Backspace => {
                let parent = files
                    .path
                    .rsplit_once('/')
                    .map_or("", |(parent, _)| parent)
                    .to_string();
                if !files.path.is_empty() {
                    self.request_files(parent);
                }
            }
            KeyCode::Enter if !files.loading => {
                if let Some(entry) = files
                    .entries
                    .get(files.selected)
                    .filter(|entry| entry.kind == WorkspaceEntryKind::Directory)
                {
                    let path = if files.path.is_empty() {
                        entry.name.clone()
                    } else {
                        format!("{}/{}", files.path, entry.name)
                    };
                    self.request_files(path);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => files.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => files.move_by(1),
            KeyCode::PageUp => files.move_by(-(self.term_size.1.saturating_sub(5).max(1) as isize)),
            KeyCode::PageDown => files.move_by(self.term_size.1.saturating_sub(5).max(1) as isize),
            KeyCode::Home => files.selected = 0,
            KeyCode::End => files.selected = files.entries.len().saturating_sub(1),
            _ => {}
        }
        self.dirty = true;
        true
    }

    pub(crate) fn on_files_mouse(&mut self, mouse: MouseEvent) -> bool {
        let Some(files) = self.files.as_mut() else {
            return false;
        };
        match mouse.kind {
            MouseEventKind::ScrollUp => files.move_by(-3),
            MouseEventKind::ScrollDown => files.move_by(3),
            _ => {}
        }
        self.dirty = true;
        true
    }
}

pub(crate) async fn fetch(
    socket: &std::path::Path,
    identity: SessionIdentity,
    path: String,
) -> Result<Frame, String> {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let (read, write) = crate::platform::connect_stream(socket).await?;
        let mut reader = FrameReader::new(read);
        let mut writer = FrameWriter::new(write);
        asd_client::handshake(&mut writer, &mut reader, ClientKind::Tui).await?;
        writer
            .write_frame(&Frame::ListWorkspaceFiles { identity, path })
            .await
            .map_err(|e| e.to_string())?;
        match reader.read_frame().await.map_err(|e| e.to_string())? {
            Some(Frame::Error { msg, .. }) => Err(msg),
            Some(frame) => Ok(frame),
            None => Err("daemon closed file-list connection".into()),
        }
    })
    .await
    .map_err(|_| "file listing timed out".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KeyModifiers;
    use crate::conn::Ev;
    use crate::graph_overlay::tests::{app_watching_commands, session};

    fn key(app: &mut App, code: KeyCode) {
        app.on_key(CtKey::new(code, KeyModifiers::NONE));
    }

    fn reply(app: &mut App, root: &str, entries: Vec<WorkspaceEntry>, truncated: bool) {
        let files = app.files.as_ref().unwrap();
        app.receive_files(
            files.identity,
            files.request,
            Ok(Frame::WorkspaceFiles {
                identity: files.identity,
                root: root.into(),
                path: files.path.clone(),
                entries,
                truncated,
            }),
        );
    }

    fn entry(name: &str, kind: WorkspaceEntryKind) -> WorkspaceEntry {
        WorkspaceEntry {
            name: name.into(),
            kind,
        }
    }

    fn open() -> (App, tokio::sync::mpsc::UnboundedReceiver<Cmd>) {
        let (mut app, commands) = app_watching_commands();
        app.sessions = vec![session("work", 42)];
        app.active = Some("work".into());
        app.on_key(CtKey::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        key(&mut app, KeyCode::Char('f'));
        (app, commands)
    }

    #[test]
    fn navigation_types_hidden_names_and_rendering() {
        let (mut app, mut commands) = open();
        assert!(matches!(commands.try_recv().unwrap(), Cmd::Files {path, ..} if path.is_empty()));
        reply(
            &mut app,
            "/remote/project",
            vec![
                entry(".hidden", WorkspaceEntryKind::File),
                entry("src", WorkspaceEntryKind::Directory),
                entry("link", WorkspaceEntryKind::Symlink),
                entry("bad\nname", WorkspaceEntryKind::Other),
            ],
            true,
        );
        assert_eq!(app.files.as_ref().unwrap().entries[0].name, "src");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 15)).unwrap();
        terminal
            .draw(|frame| app.files.as_ref().unwrap().draw(frame))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        for text in [
            "/remote/project",
            "[dir",
            "[file",
            "[link",
            "[other",
            ".hidden",
            "bad\\nname",
            "Listing truncated",
            "read-only",
        ] {
            assert!(rendered.contains(text), "missing {text}");
        }
        key(&mut app, KeyCode::Enter);
        assert!(matches!(commands.try_recv().unwrap(), Cmd::Files {path, ..} if path == "src"));
        reply(&mut app, "/remote/project", Vec::new(), false);
        key(&mut app, KeyCode::Backspace);
        assert!(matches!(commands.try_recv().unwrap(), Cmd::Files {path, ..} if path.is_empty()));
        key(&mut app, KeyCode::Backspace);
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn overlay_swallows_keys_paste_mouse_and_cancels_on_close() {
        let (mut app, mut commands) = open();
        commands.try_recv().unwrap();
        reply(
            &mut app,
            "/remote/project",
            vec![
                entry("link", WorkspaceEntryKind::Symlink),
                entry("file", WorkspaceEntryKind::File),
            ],
            false,
        );
        for code in [
            KeyCode::Enter,
            KeyCode::End,
            KeyCode::Enter,
            KeyCode::Char('x'),
        ] {
            key(&mut app, code);
        }
        app.on_paste("rm -rf /\n");
        app.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(crate::MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            ratatui::layout::Size::new(80, 24),
        );
        assert!(commands.try_recv().is_err());
        let old = app.files.as_ref().unwrap().request;
        let identity = app.files.as_ref().unwrap().identity;
        key(&mut app, KeyCode::Esc);
        assert!(matches!(commands.try_recv().unwrap(), Cmd::CancelFiles));
        app.receive_files(identity, old, Err("late".into()));
        assert!(app.files.is_none());
    }

    #[test]
    fn refresh_rejects_old_generation_and_changed_root_requires_explicit_refresh() {
        let (mut app, _) = open();
        reply(
            &mut app,
            "/first",
            vec![entry("src", WorkspaceEntryKind::Directory)],
            false,
        );
        let identity = app.files.as_ref().unwrap().identity;
        let old = app.files.as_ref().unwrap().request;
        key(&mut app, KeyCode::Enter);
        app.receive_files(identity, old, Err("stale".into()));
        assert!(app.files.as_ref().unwrap().loading);
        reply(
            &mut app,
            "/second",
            vec![entry("must-not-show", WorkspaceEntryKind::File)],
            false,
        );
        let files = app.files.as_ref().unwrap();
        assert!(files.entries.is_empty());
        assert!(files.path.is_empty());
        assert!(files.message.contains("Workspace changed"));
        key(&mut app, KeyCode::Char('r'));
        reply(&mut app, "/second", Vec::new(), false);
        assert_eq!(app.files.as_ref().unwrap().message, "Empty directory");
    }

    #[test]
    fn lifecycle_rejects_recreated_session_and_disconnect() {
        let (mut app, _) = open();
        let identity = app.files.as_ref().unwrap().identity;
        let request = app.files.as_ref().unwrap().request;
        app.sessions[0].instance_id += 1;
        app.receive_files(identity, request, Err("stale".into()));
        assert!(app.files.is_none());
        app.toggle_files();
        app.on_conn_event(Ev::Down("lost".into()));
        assert!(app.files.is_none());
        let (mut app, _) = open();
        app.on_conn_event(Ev::Sessions(Vec::new()));
        assert!(app.files.is_none());
    }

    #[test]
    fn scrolling_reaches_tail_and_overlay_opening_is_exclusive() {
        let (mut app, _) = open();
        reply(
            &mut app,
            "/remote",
            (0..100)
                .map(|i| entry(&format!("{i:03}"), WorkspaceEntryKind::File))
                .collect(),
            false,
        );
        key(&mut app, KeyCode::End);
        assert_eq!(app.files.as_ref().unwrap().selected, 99);
        key(&mut app, KeyCode::PageUp);
        assert!(app.files.as_ref().unwrap().selected < 99);
        key(&mut app, KeyCode::Home);
        assert_eq!(app.files.as_ref().unwrap().selected, 0);
        app.toggle_review();
        assert!(app.files.is_none());
        assert!(app.review.is_some());
        app.toggle_files();
        assert!(app.review.is_none());
        assert!(app.files.is_some());
    }
}
