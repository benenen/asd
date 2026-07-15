//! Settings overlay: a modal panel with a category sidebar (General /
//! Connections) and a content area. The Connections page shows saved SSH
//! connections with add / edit / delete. Modeled on the Codex desktop SSH
//! settings UI.
//!
//! This module avoids type-inference issues by using `iced::Element` with
//! explicit default renderer.

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    button, column, container, row, scrollable, text, text_input, Space,
};
use iced::{Background, Border, Color, Element, Font, Length, Padding, Theme};

use crate::theme;
use crate::theme::{
    ALERT, BRIGHT, DIM, HOVER, LINE, LINE_SOFT, LOCAL, MUTED, PANEL, REMOTE,
    SCREEN, TEXT,
};

use serde::{Deserialize, Serialize};

// ── config persistence ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SshConnection {
    pub name: String,
    pub host: String,
    pub user: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_port() -> u16 {
    22
}

impl SshConnection {
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
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
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

// ── UI state ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    General,
    Connections,
}

#[derive(Debug, Clone)]
pub struct SshForm {
    pub index: Option<usize>,
    pub name: String,
    pub host: String,
    pub user: String,
    pub port: String,
}

impl Default for SshForm {
    fn default() -> Self {
        Self {
            index: None,
            name: String::new(),
            host: String::new(),
            user: String::new(),
            port: String::from("22"),
        }
    }
}

impl SshForm {
    pub(crate) fn from_conn(c: &SshConnection, i: usize) -> Self {
        Self {
            index: Some(i),
            name: c.name.clone(),
            host: c.host.clone(),
            user: c.user.clone(),
            port: c.port.to_string(),
        }
    }

    pub(crate) fn valid(&self) -> bool {
        !self.host.trim().is_empty()
            && !self.user.trim().is_empty()
            && self.port.trim().parse::<u16>().is_ok()
    }

    pub(crate) fn into_connection(&self) -> Option<SshConnection> {
        if !self.valid() {
            return None;
        }
        let name = if self.name.trim().is_empty() {
            format!("{}@{}", self.user.trim(), self.host.trim())
        } else {
            self.name.trim().to_string()
        };
        Some(SshConnection {
            name,
            host: self.host.trim().to_string(),
            user: self.user.trim().to_string(),
            port: self.port.trim().parse().unwrap_or(22),
        })
    }
}

// ── messages ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum SettingsMsg {
    Close,
    Nav(SettingsPage),
    AddConnection,
    EditConnection(usize),
    DeleteConnection(usize),
    SaveConnection,
    CancelEdit,
    FormName(String),
    FormHost(String),
    FormUser(String),
    FormPort(String),
}

// ── layout constants ──────────────────────────────────────────────────

const NAV_W: f32 = 180.0;
const PANEL_W: f32 = 680.0;
const PANEL_H: f32 = 480.0;

fn pad2(v: f32, h: f32) -> Padding {
    Padding {
        top: v,
        right: h,
        bottom: v,
        left: h,
    }
}
fn pad(top: f32, right: f32, bottom: f32, left: f32) -> Padding {
    Padding {
        top,
        right,
        bottom,
        left,
    }
}
fn mono() -> Font {
    Font::MONOSPACE
}
fn bold() -> Font {
    Font {
        weight: iced::font::Weight::Bold,
        ..Font::MONOSPACE
    }
}
fn spacer() -> Space {
    Space::new().width(Length::Fill)
}

// ── public entry ──────────────────────────────────────────────────────

/// The settings panel: sidebar + content in a rounded card.  Caller
/// (view.rs) is responsible for backdrop + centering.
pub fn view<'a>(
    page: SettingsPage,
    connections: &'a [SshConnection],
    form: &'a Option<SshForm>,
) -> Element<'a, SettingsMsg> {
    let inner = row![nav(page), content(page, connections, form)]
        .height(Length::Fixed(PANEL_H));

    container(inner)
        .width(Length::Fixed(PANEL_W))
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(SCREEN)),
            border: Border {
                color: LINE,
                width: 1.0,
                radius: 12.0.into(),
            },
            ..Default::default()
        })
        .into()
}

// ── nav sidebar ───────────────────────────────────────────────────────

fn nav(active: SettingsPage) -> Element<'static, SettingsMsg> {
    let header = container(text("Settings").size(15).font(bold()).color(BRIGHT))
        .padding(pad(24.0, 20.0, 16.0, 20.0));

    let items = column![
        nav_item("General", SettingsPage::General, active),
        nav_item("Connections", SettingsPage::Connections, active),
    ]
    .spacing(2);

    let inner = column![header, items, spacer()].height(Length::Fill);

    container(inner)
        .width(Length::Fixed(NAV_W))
        .height(Length::Fill)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(PANEL)),
            border: Border {
                radius: 12.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

fn nav_item<'a>(
    label: &'a str,
    page: SettingsPage,
    active: SettingsPage,
) -> Element<'a, SettingsMsg> {
    let is_active = active == page;
    let fg = if is_active { BRIGHT } else { MUTED };
    let bg = if is_active {
        Some(Background::Color(theme::tint(LOCAL, 0.12)))
    } else {
        None
    };

    let label_col = text(label).size(13).font(mono());
    let item = if is_active {
        row![
            container(text(""))
                .width(Length::Fixed(3.0))
                .height(Length::Fill)
                .style(move |_: &Theme| container::Style {
                    background: Some(Background::Color(LOCAL)),
                    border: Border {
                        radius: 2.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
            label_col,
        ]
    } else {
        row![
            container(text("")).width(Length::Fixed(3.0)),
            label_col,
        ]
    };

    button(item.spacing(10).align_y(Vertical::Center))
        .width(Length::Fill)
        .padding(pad2(9.0, 20.0))
        .on_press(SettingsMsg::Nav(page))
        .style(move |_: &Theme, status| button::Style {
            background: if is_active {
                bg
            } else if matches!(status, button::Status::Hovered | button::Status::Pressed)
            {
                Some(Background::Color(HOVER))
            } else {
                None
            },
            text_color: fg,
            border: Border::default(),
            ..Default::default()
        })
        .into()
}

// ── content area ──────────────────────────────────────────────────────

fn content<'a>(
    page: SettingsPage,
    connections: &'a [SshConnection],
    form: &'a Option<SshForm>,
) -> Element<'a, SettingsMsg> {
    match page {
        SettingsPage::General => general_page(),
        SettingsPage::Connections => connections_page(connections, form),
    }
}

fn general_page() -> Element<'static, SettingsMsg> {
    let title = text("General").size(16).font(bold()).color(BRIGHT);
    let body = column![
        setting_row("App", "asd GPU Terminal Client"),
        setting_row("Version", env!("CARGO_PKG_VERSION")),
        setting_row("Protocol", "v2"),
    ]
    .spacing(4);

    let p = pad2(10.0, 20.0);
    container(column![container(title).padding(p), container(body).padding(p)])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn setting_row<'a>(label: &'a str, value: &'a str) -> Element<'a, SettingsMsg> {
    row![
        text(label)
            .size(12)
            .font(mono())
            .color(MUTED)
            .width(Length::Fixed(80.0)),
        text(value).size(12).font(mono()).color(TEXT),
    ]
    .spacing(12)
    .into()
}

// ── connections page ──────────────────────────────────────────────────

fn connections_page<'a>(
    connections: &'a [SshConnection],
    form: &'a Option<SshForm>,
) -> Element<'a, SettingsMsg> {
    let add_btn: Element<'a, SettingsMsg> = button(
        text("+ Add").size(12).font(bold()).color(LOCAL),
    )
    .padding(pad2(5.0, 12.0))
    .on_press(SettingsMsg::AddConnection)
    .style(move |_: &Theme, status| btn_outline(LOCAL, status))
    .into();

    let title = row![
        text("SSH Connections").size(16).font(bold()).color(BRIGHT),
        spacer(),
        if form.is_none() {
            add_btn
        } else {
            text("").into()
        },
    ]
    .align_y(Vertical::Center);

    let body: Element<'a, SettingsMsg> = if let Some(f) = form {
        connection_form(f, connections)
    } else if connections.is_empty() {
        container(
            column![
                text("No saved connections")
                    .size(13)
                    .font(mono())
                    .color(MUTED),
                text("Add an SSH host to quickly connect from the main sidebar.")
                    .size(12)
                    .font(mono())
                    .color(DIM),
            ]
            .spacing(8)
            .align_x(Horizontal::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Horizontal::Center)
        .align_y(Vertical::Center)
        .into()
    } else {
        let mut list = column![].spacing(4);
        for (i, conn) in connections.iter().enumerate() {
            list = list.push(connection_row(i, conn));
        }
        scrollable(list).height(Length::Fill).into()
    };

    let p = pad2(10.0, 20.0);
    container(column![
        container(title).padding(p),
        container(body).padding(pad2(4.0, 20.0)).height(Length::Fill),
    ])
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn connection_row<'a>(index: usize, conn: &'a SshConnection) -> Element<'a, SettingsMsg> {
    let dot = container(text(""))
        .width(Length::Fixed(8.0))
        .height(Length::Fixed(8.0))
        .style(move |_: &Theme| container::Style {
            background: Some(Background::Color(REMOTE)),
            border: Border {
                radius: 4.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });

    let info = column![
        text(&conn.name).size(13).font(bold()).color(TEXT),
        text(conn.label()).size(11).font(mono()).color(MUTED),
    ]
    .spacing(2);

    let actions = row![
        icon_btn("\u{270e}", SettingsMsg::EditConnection(index), MUTED),
        icon_btn("\u{2715}", SettingsMsg::DeleteConnection(index), ALERT),
    ]
    .spacing(6);

    button(row![dot, info.width(Length::Fill), actions].spacing(10).align_y(Vertical::Center))
        .width(Length::Fill)
        .padding(pad2(8.0, 12.0))
        .style(move |_: &Theme, status| btn_row_style(status))
        .into()
}

fn connection_form<'a>(
    form: &'a SshForm,
    connections: &'a [SshConnection],
) -> Element<'a, SettingsMsg> {
    let is_edit = form.index.is_some();
    let heading = if is_edit {
        "Edit Connection"
    } else {
        "New Connection"
    };

    let lbl = form_field("Name", &form.name, SettingsMsg::FormName, "My GPU Server");
    let hst = form_field("Host", &form.host, SettingsMsg::FormHost, "gpu-01.example.com");
    let usr = form_field(
        "User",
        &form.user,
        SettingsMsg::FormUser,
        "your-username",
    );
    let prt = form_field("Port", &form.port, SettingsMsg::FormPort, "22");

    let valid = form.valid()
        && !connections.iter().enumerate().any(|(i, c)| {
            if form.index == Some(i) {
                return false;
            }
            c.host == form.host.trim()
                && c.user == form.user.trim()
                && c.port.to_string() == form.port.trim()
        });

    let actions = row![
        button(text("Cancel").size(12).font(mono()).color(MUTED))
            .padding(pad2(6.0, 14.0))
            .on_press(SettingsMsg::CancelEdit)
            .style(move |_: &Theme, status| btn_outline(MUTED, status)),
        if valid {
            let b: Element<'a, SettingsMsg> = button(
                text("Save").size(12).font(bold()).color(SCREEN),
            )
            .padding(pad2(6.0, 16.0))
            .on_press(SettingsMsg::SaveConnection)
            .style(move |_: &Theme, status| btn_filled(LOCAL, status))
            .into();
            b
        } else {
            let b: Element<'a, SettingsMsg> = button(
                text("Save").size(12).font(bold()).color(DIM),
            )
            .padding(pad2(6.0, 16.0))
            .style(move |_: &Theme, _| btn_filled(DIM, button::Status::Active))
            .into();
            b
        },
    ]
    .spacing(10);

    container(
        column![
            text(heading).size(14).font(bold()).color(BRIGHT),
            column![lbl, hst, usr, prt].spacing(12),
            actions,
        ]
        .spacing(16),
    )
    .padding(pad2(16.0, 0.0))
    .into()
}

fn form_field<'a>(
    label: &'a str,
    value: &'a str,
    msg: fn(String) -> SettingsMsg,
    placeholder: &'a str,
) -> Element<'a, SettingsMsg> {
    column![
        text(label).size(11).font(mono()).color(MUTED),
        text_input(placeholder, value)
            .on_input(msg)
            .font(mono())
            .size(13)
            .padding(pad2(8.0, 10.0))
            .style(|_, _| text_input::Style {
                background: Background::Color(PANEL),
                border: Border {
                    color: LINE_SOFT,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                icon: DIM,
                placeholder: DIM,
                value: TEXT,
                selection: theme::tint(LOCAL, 0.3),
            }),
    ]
    .spacing(4)
    .into()
}

// ── small icon button ─────────────────────────────────────────────────

fn icon_btn<'a>(glyph: &'a str, msg: SettingsMsg, color: Color) -> Element<'a, SettingsMsg> {
    button(text(glyph.to_string()).size(14).font(mono()).color(color))
        .padding(pad2(3.0, 7.0))
        .on_press(msg)
        .style(move |_: &Theme, status| button::Style {
            background: matches!(
                status,
                button::Status::Hovered | button::Status::Pressed
            )
            .then_some(Background::Color(HOVER)),
            text_color: color,
            border: Border {
                radius: 4.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

// ── button styles ─────────────────────────────────────────────────────

fn btn_outline(color: Color, status: button::Status) -> button::Style {
    button::Style {
        background: matches!(status, button::Status::Hovered | button::Status::Pressed)
            .then_some(Background::Color(theme::tint(color, 0.12))),
        text_color: color,
        border: Border {
            color,
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    }
}

fn btn_filled(color: Color, status: button::Status) -> button::Style {
    button::Style {
        background: Some(Background::Color(
            if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                theme::tint(color, 0.8)
            } else {
                color
            },
        )),
        text_color: SCREEN,
        border: Border {
            color,
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    }
}

fn btn_row_style(status: button::Status) -> button::Style {
    button::Style {
        background: matches!(status, button::Status::Hovered | button::Status::Pressed)
            .then_some(Background::Color(HOVER)),
        text_color: TEXT,
        border: Border {
            color: LINE_SOFT,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    }
}
