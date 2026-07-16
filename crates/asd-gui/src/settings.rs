//! Settings overlay: a modal panel with a category sidebar (General /
//! Connections) and a content area. The Connections page shows saved SSH
//! connections with add / edit / delete. Modeled on the Codex desktop SSH
//! settings UI.
//!
//! This module avoids type-inference issues by using `iced::Element` with
//! explicit default renderer.

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{Space, button, column, container, row, scrollable, text, text_input};
use iced::{Background, Border, Color, Element, Font, Length, Padding, Theme};

use crate::theme;
use crate::theme::{
    ALERT, BRIGHT, DIM, HOVER, LINE, LINE_SOFT, LOCAL, MUTED, PANEL, REMOTE, SCREEN, TEXT,
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
    /// How to authenticate to this host. Defaults to key-based so existing
    /// configs (written before this field existed) keep working.
    #[serde(default)]
    pub auth: SshAuth,
}

fn default_port() -> u16 {
    22
}

/// How a saved connection authenticates. `Password` stores the password inline;
/// `Key` names a private-key file (empty path = try the default `~/.ssh` keys),
/// with an optional passphrase.
///
/// Note: secrets are persisted in the local config file (`config.json`) in
/// plain text — same trust model as `~/.ssh` on a single-user machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum SshAuth {
    Password {
        password: String,
    },
    Key {
        key_path: String,
        passphrase: String,
    },
}

impl Default for SshAuth {
    fn default() -> Self {
        Self::Key {
            key_path: String::new(),
            passphrase: String::new(),
        }
    }
}

impl SshAuth {
    fn kind(&self) -> AuthKind {
        match self {
            Self::Password { .. } => AuthKind::Password,
            Self::Key { .. } => AuthKind::Key,
        }
    }

    /// One-word tag for the connection list ("password" / "key").
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Password { .. } => "password",
            Self::Key { .. } => "key",
        }
    }
}

/// The two authentication choices offered in the form's segmented toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    Password,
    Key,
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
    pub auth_kind: AuthKind,
    pub password: String,
    pub key_path: String,
    pub passphrase: String,
}

impl Default for SshForm {
    fn default() -> Self {
        Self {
            index: None,
            name: String::new(),
            host: String::new(),
            user: String::new(),
            port: String::from("22"),
            auth_kind: AuthKind::Key,
            password: String::new(),
            key_path: String::new(),
            passphrase: String::new(),
        }
    }
}

impl SshForm {
    pub(crate) fn from_conn(c: &SshConnection, i: usize) -> Self {
        let (password, key_path, passphrase) = match &c.auth {
            SshAuth::Password { password } => (password.clone(), String::new(), String::new()),
            SshAuth::Key {
                key_path,
                passphrase,
            } => (String::new(), key_path.clone(), passphrase.clone()),
        };
        Self {
            index: Some(i),
            name: c.name.clone(),
            host: c.host.clone(),
            user: c.user.clone(),
            port: c.port.to_string(),
            auth_kind: c.auth.kind(),
            password,
            key_path,
            passphrase,
        }
    }

    /// The first reason the form can't be saved, phrased for the user, or
    /// `None` when it is valid. Drives both the disabled Save button and the
    /// inline hint.
    pub(crate) fn invalid_reason(&self) -> Option<&'static str> {
        if self.name.trim().is_empty() {
            return Some("Name is required.");
        }
        if self.host.trim().is_empty() {
            return Some("Host is required.");
        }
        if self.user.trim().is_empty() {
            return Some("User is required.");
        }
        if self.port.trim().parse::<u16>().is_err() {
            return Some("Port must be a number (1–65535).");
        }
        if self.auth_kind == AuthKind::Password && self.password.is_empty() {
            return Some("Password is required.");
        }
        None
    }

    pub(crate) fn valid(&self) -> bool {
        self.invalid_reason().is_none()
    }

    fn auth(&self) -> SshAuth {
        match self.auth_kind {
            AuthKind::Password => SshAuth::Password {
                password: self.password.clone(),
            },
            AuthKind::Key => SshAuth::Key {
                key_path: self.key_path.trim().to_string(),
                passphrase: self.passphrase.clone(),
            },
        }
    }

    // Borrows rather than consumes (the form stays editable), despite the name.
    #[allow(clippy::wrong_self_convention)]
    pub(crate) fn into_connection(&self) -> Option<SshConnection> {
        if !self.valid() {
            return None;
        }
        Some(SshConnection {
            name: self.name.trim().to_string(),
            host: self.host.trim().to_string(),
            user: self.user.trim().to_string(),
            port: self.port.trim().parse().unwrap_or(22),
            auth: self.auth(),
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
    FormAuthKind(AuthKind),
    FormPassword(String),
    FormKeyPath(String),
    FormPassphrase(String),
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
    let inner = row![nav(page), content(page, connections, form)].height(Length::Fixed(PANEL_H));

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

    // A full-height 3px rail — colored only when active — is present on every
    // item so the label is vertically centered whether or not it is selected
    // (a Fill-height child lets `align_y(Center)` work; without it an inactive
    // item's row shrinks to the text and sits at the top).
    let rail = container(text(""))
        .width(Length::Fixed(3.0))
        .height(Length::Fill)
        .style(move |_: &Theme| container::Style {
            background: is_active.then_some(Background::Color(LOCAL)),
            border: Border {
                radius: 2.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });
    let item = row![rail, text(label).size(13).font(mono())]
        .spacing(10)
        .align_y(Vertical::Center);

    // Fixed height: the accent rail is `height(Fill)`, which would otherwise
    // stretch the item to consume the whole nav column.
    button(item)
        .width(Length::Fill)
        .height(Length::Fixed(38.0))
        .padding(pad2(0.0, 20.0))
        .on_press(SettingsMsg::Nav(page))
        .style(move |_: &Theme, status| button::Style {
            background: if is_active {
                bg
            } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
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
    container(column![
        container(title).padding(p),
        container(body).padding(p)
    ])
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
    let add_btn: Element<'a, SettingsMsg> =
        button(text("+ Add").size(12).font(bold()).color(REMOTE))
            .padding(pad2(5.0, 12.0))
            .on_press(SettingsMsg::AddConnection)
            .style(move |_: &Theme, status| btn_outline(REMOTE, status))
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
        container(body)
            .padding(pad2(4.0, 20.0))
            .height(Length::Fill),
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

    button(
        row![
            dot,
            info.width(Length::Fill),
            auth_pill(conn.auth.tag()),
            actions
        ]
        .spacing(10)
        .align_y(Vertical::Center),
    )
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
        "Edit connection"
    } else {
        "New connection"
    };

    // Identity: Name (required), Host + Port on one row, User.
    let host_port = row![
        container(field(
            "Host",
            &form.host,
            SettingsMsg::FormHost,
            "gpu-01.example.com",
            true,
            false,
        ))
        .width(Length::Fill),
        container(field(
            "Port",
            &form.port,
            SettingsMsg::FormPort,
            "22",
            false,
            false,
        ))
        .width(Length::Fixed(96.0)),
    ]
    .spacing(12);

    // Authentication: segmented toggle + method-specific fields.
    let auth_fields: Element<'a, SettingsMsg> = match form.auth_kind {
        AuthKind::Password => field(
            "Password",
            &form.password,
            SettingsMsg::FormPassword,
            "required",
            true,
            true,
        ),
        AuthKind::Key => column![
            field(
                "Private key",
                &form.key_path,
                SettingsMsg::FormKeyPath,
                "~/.ssh/id_ed25519   ·   blank = default keys",
                false,
                false,
            ),
            field(
                "Passphrase",
                &form.passphrase,
                SettingsMsg::FormPassphrase,
                "optional",
                false,
                true,
            ),
        ]
        .spacing(12)
        .into(),
    };

    let auth = column![
        section_label("AUTHENTICATION"),
        auth_toggle(form.auth_kind),
        auth_fields,
    ]
    .spacing(10);

    let fields = column![
        field(
            "Name",
            &form.name,
            SettingsMsg::FormName,
            "My GPU server",
            true,
            false,
        ),
        host_port,
        field(
            "User",
            &form.user,
            SettingsMsg::FormUser,
            "root",
            true,
            false,
        ),
        divider(),
        auth,
    ]
    .spacing(14);

    // Validation: field-level reason first, then the duplicate-host check.
    let dup = connections.iter().enumerate().any(|(i, c)| {
        form.index != Some(i)
            && c.host == form.host.trim()
            && c.user == form.user.trim()
            && c.port.to_string() == form.port.trim()
    });
    let reason: Option<&str> = form
        .invalid_reason()
        .or(dup.then_some("A connection to this host already exists."));
    let valid = reason.is_none();

    let save: Element<'a, SettingsMsg> = if valid {
        button(text("Save").size(12).font(bold()).color(SCREEN))
            .padding(pad2(6.0, 18.0))
            .on_press(SettingsMsg::SaveConnection)
            .style(move |_: &Theme, status| btn_filled(REMOTE, status))
            .into()
    } else {
        button(text("Save").size(12).font(bold()).color(DIM))
            .padding(pad2(6.0, 18.0))
            .style(move |_: &Theme, _| btn_filled(DIM, button::Status::Active))
            .into()
    };

    let hint: Element<'a, SettingsMsg> = match reason {
        Some(r) => text(r).size(11).font(mono()).color(MUTED).into(),
        None => text("").into(),
    };

    let actions = row![
        hint,
        spacer(),
        button(text("Cancel").size(12).font(mono()).color(MUTED))
            .padding(pad2(6.0, 14.0))
            .on_press(SettingsMsg::CancelEdit)
            .style(move |_: &Theme, status| btn_outline(MUTED, status)),
        save,
    ]
    .spacing(10)
    .align_y(Vertical::Center);

    container(
        column![
            text(heading).size(14).font(bold()).color(BRIGHT),
            scrollable(fields).height(Length::Fill),
            actions,
        ]
        .spacing(14),
    )
    .height(Length::Fill)
    .padding(pad2(4.0, 0.0))
    .into()
}

/// A labeled text field. `required` adds a cyan asterisk; `secure` masks input.
fn field<'a>(
    label: &'a str,
    value: &'a str,
    msg: fn(String) -> SettingsMsg,
    placeholder: &'a str,
    required: bool,
    secure: bool,
) -> Element<'a, SettingsMsg> {
    let mut head = row![text(label).size(11).font(mono()).color(MUTED)].spacing(3);
    if required {
        head = head.push(text("*").size(11).font(bold()).color(REMOTE));
    }
    column![
        head.align_y(Vertical::Center),
        text_input(placeholder, value)
            .on_input(msg)
            .secure(secure)
            .font(mono())
            .size(13)
            .padding(pad2(8.0, 10.0))
            .style(field_style),
    ]
    .spacing(4)
    .into()
}

/// Text-input styling: cyan focus ring, matching the remote accent.
fn field_style(_: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused { .. });
    text_input::Style {
        background: Background::Color(PANEL),
        border: Border {
            color: if focused { REMOTE } else { LINE_SOFT },
            width: 1.0,
            radius: 6.0.into(),
        },
        icon: DIM,
        placeholder: DIM,
        value: TEXT,
        selection: theme::tint(REMOTE, 0.3),
    }
}

/// An uppercase eyebrow label for a form section.
fn section_label(s: &'static str) -> Element<'static, SettingsMsg> {
    text(s).size(10).font(bold()).color(DIM).into()
}

/// A 1px hairline separating form sections.
fn divider() -> Element<'static, SettingsMsg> {
    container(Space::new().height(Length::Fixed(1.0)).width(Length::Fill))
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(LINE_SOFT)),
            ..Default::default()
        })
        .into()
}

/// The Password | Key segmented toggle. The active segment is filled with the
/// remote (cyan) accent to match the rest of the connection UI.
fn auth_toggle(active: AuthKind) -> Element<'static, SettingsMsg> {
    row![
        seg_btn("Password", AuthKind::Password, active),
        seg_btn("Key", AuthKind::Key, active),
    ]
    .spacing(8)
    .into()
}

fn seg_btn(label: &'static str, kind: AuthKind, active: AuthKind) -> Element<'static, SettingsMsg> {
    let is = kind == active;
    button(
        text(label)
            .size(12)
            .font(if is { bold() } else { mono() })
            .color(if is { SCREEN } else { MUTED }),
    )
    .padding(pad2(6.0, 16.0))
    .on_press(SettingsMsg::FormAuthKind(kind))
    .style(move |_: &Theme, status| {
        if is {
            btn_filled(REMOTE, status)
        } else {
            btn_outline(MUTED, status)
        }
    })
    .into()
}

/// Small pill showing a saved connection's auth method ("key" / "password").
fn auth_pill(tag: &'static str) -> Element<'static, SettingsMsg> {
    container(text(tag).size(9).font(mono()).color(REMOTE))
        .padding(pad2(1.0, 6.0))
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(theme::tint(REMOTE, 0.1))),
            border: Border {
                color: theme::tint(REMOTE, 0.35),
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        })
        .into()
}

// ── small icon button ─────────────────────────────────────────────────

fn icon_btn<'a>(glyph: &'a str, msg: SettingsMsg, color: Color) -> Element<'a, SettingsMsg> {
    button(text(glyph.to_string()).size(14).font(mono()).color(color))
        .padding(pad2(3.0, 7.0))
        .on_press(msg)
        .style(move |_: &Theme, status| button::Style {
            background: matches!(status, button::Status::Hovered | button::Status::Pressed)
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
