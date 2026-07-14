//! The two-pane shell (spec §7, M2 redesign): a host-grouped session sidebar
//! beside the live terminal, with a full-width status bar. Modeled on boo's
//! `boo ui`. Host origin is encoded by the accent rail — amber = local, cyan =
//! SSH remote (see [`crate::theme`]).

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    Space, button, canvas, column, container, mouse_area, row, scrollable, text, text_input,
};
use iced::{Background, Border, Color, Element, Font, Length, Padding};

use crate::model::{Host, HostState, Model};
use crate::render::TermCanvas;
use crate::{App, Message, Status, model, theme};

/// A flexible horizontal gap that pushes following items to the right.
fn spacer() -> Space {
    Space::new().width(Length::Fill)
}

/// Sidebar width in logical pixels; the grid math subtracts it from the window.
pub const SIDEBAR_W: f32 = 262.0;
/// Status-bar height.
pub const STATUS_H: f32 = 28.0;
/// Terminal-header height.
pub const TERMHEAD_H: f32 = 40.0;

/// Asymmetric padding `(top, right, bottom, left)`, built explicitly to avoid
/// any ambiguity over which `From<[…]>` impl applies.
fn pad(top: f32, right: f32, bottom: f32, left: f32) -> Padding {
    Padding {
        top,
        right,
        bottom,
        left,
    }
}
/// Vertical/horizontal padding.
fn pad2(v: f32, h: f32) -> Padding {
    Padding {
        top: v,
        right: h,
        bottom: v,
        left: h,
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

/// The whole window: sidebar | terminal, over a full-width status bar.
pub fn view(app: &App) -> Element<'_, Message> {
    let body = row![sidebar(app), vline(), terminal_pane(app)].height(Length::Fill);
    column![
        container(body).height(Length::Fill),
        hline(theme::LINE),
        status_bar(app),
    ]
    .into()
}

// ------------------------------------------------------------------ sidebar

fn sidebar(app: &App) -> Element<'_, Message> {
    let m = &app.model;

    let brand = row![keycap('a'), keycap('s'), keycap('d'), brand_tag(m)]
        .spacing(4)
        .align_y(Vertical::Center);

    let mut groups = column![].spacing(2).padding(pad2(4.0, 0.0));
    for host in &m.hosts {
        groups = groups.push(host_group(host, m, app.now_ms));
    }
    groups = groups.push(connect_remote(&app.remote_input));

    let scroll = scrollable(groups).height(Length::Fill);

    let inner = column![
        container(brand).padding(pad2(16.0, 16.0)),
        label("HOSTS"),
        scroll,
        hline(theme::LINE),
        sidebar_foot(m),
    ]
    .height(Length::Fill);

    container(inner)
        .width(Length::Fixed(SIDEBAR_W))
        .height(Length::Fill)
        .style(|_| panel(theme::PANEL))
        .into()
}

fn brand_tag(m: &Model) -> Element<'_, Message> {
    let sub = format!(
        "{} session{} · {} host{}",
        m.total_sessions(),
        plural(m.total_sessions()),
        m.hosts.len(),
        plural(m.hosts.len()),
    );
    column![
        text("session pool").size(10).font(mono()).color(theme::DIM),
        text(sub).size(11).font(mono()).color(theme::MUTED),
    ]
    .spacing(1)
    .into()
}

fn host_group<'a>(host: &'a Host, m: &'a Model, now_ms: u64) -> Element<'a, Message> {
    let head = host_head(host);

    let accent = theme::rail(host.is_remote());
    let mut rows = column![].spacing(1);
    for s in &host.sessions {
        rows = rows.push(session_row(host, s, m.is_active(host.id, &s.name), now_ms));
    }
    // The rail: a 2px accent spine the height of the session list.
    let rail = container(text(""))
        .width(Length::Fixed(2.0))
        .height(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(accent)),
            border: Border {
                radius: 2.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });
    let body = row![rail, rows.width(Length::Fill)]
        .spacing(10)
        .padding(pad(0.0, 0.0, 0.0, 14.0));

    column![head, body]
        .spacing(2)
        .padding(pad2(2.0, 10.0))
        .into()
}

fn host_head(host: &Host) -> Element<'_, Message> {
    let (dot_color, filled, tag) = match &host.state {
        HostState::Up => (theme::OK, true, None),
        HostState::Connecting => (theme::LOCAL, true, Some("connecting")),
        HostState::Down(_) => (theme::ALERT, true, Some("offline")),
    };

    let mut line = row![
        status_dot(dot_color, filled),
        text(host.label()).size(11).font(bold()).color(theme::MUTED),
        text(host.sublabel())
            .size(10)
            .font(mono())
            .color(theme::DIM),
    ]
    .spacing(7)
    .align_y(Vertical::Center);

    let right: Element<'_, Message> = if let Some(t) = tag {
        text(t).size(9).font(mono()).color(dot_color).into()
    } else {
        text(format!("{}", host.sessions.len()))
            .size(10)
            .font(mono())
            .color(theme::DIM)
            .into()
    };
    line = line.push(spacer());
    line = line.push(right);
    line = line.push(icon_button("+", Message::NewSession(host.id), theme::MUTED));
    if host.is_remote() {
        line = line.push(icon_button("×", Message::RemoveHost(host.id), theme::DIM));
    }

    container(line)
        .width(Length::Fill)
        .padding(pad2(7.0, 8.0))
        .into()
}

fn session_row<'a>(
    host: &'a Host,
    s: &'a asd_proto::SessionInfo,
    selected: bool,
    now_ms: u64,
) -> Element<'a, Message> {
    let accent = theme::rail(host.is_remote());

    let mut content = row![
        session_dot(accent, s.attached_clients > 0),
        text(s.name.as_str())
            .size(13)
            .font(bold())
            .color(if selected { theme::BRIGHT } else { theme::TEXT }),
    ]
    .spacing(9)
    .align_y(Vertical::Center);

    content = content.push(spacer());
    let cmd = model::short_cmd(&s.command);
    if !cmd.is_empty() {
        content = content.push(text(cmd).size(11).font(mono()).color(theme::MUTED));
    }
    if s.attached_clients > 1 {
        content = content.push(peers_pill(accent, s.attached_clients));
    }
    content = content.push(
        text(model::short_age(s.created_ms, now_ms))
            .size(10)
            .font(mono())
            .color(theme::DIM),
    );

    let host_id = host.id;
    let name = s.name.clone();
    // The whole row selects; a sibling ✕ kills. Siblings, not nested buttons,
    // so iced routes each click to exactly one.
    let select = button(content)
        .width(Length::Fill)
        .padding(pad2(6.0, 9.0))
        .on_press(Message::Select(host_id, name.clone()))
        .style(move |_, status| session_style(selected, accent, status));
    row![
        select,
        icon_button("×", Message::Kill(host_id, name), theme::DIM)
    ]
    .align_y(Vertical::Center)
    .into()
}

fn connect_remote(input: &str) -> Element<'_, Message> {
    let field = text_input("user@host  ↵ connect", input)
        .on_input(Message::RemoteInput)
        .on_submit(Message::RemoteSubmit)
        .font(mono())
        .size(12)
        .padding(pad2(7.0, 9.0))
        .style(|_, _| text_input::Style {
            background: Background::Color(theme::SCREEN),
            border: Border {
                color: theme::DASH,
                width: 1.0,
                radius: 8.0.into(),
            },
            icon: theme::DIM,
            placeholder: theme::DIM,
            value: theme::REMOTE,
            selection: theme::tint(theme::REMOTE, 0.3),
        });
    container(row![text("+").size(13).color(theme::REMOTE), field].spacing(8))
        .padding(pad2(6.0, 16.0))
        .into()
}

fn sidebar_foot(m: &Model) -> Element<'_, Message> {
    let up = m
        .host(model::LOCAL_ID)
        .is_some_and(|h| h.state == HostState::Up);
    let (c, msg) = if up {
        (theme::OK, "daemon up · proto v1")
    } else {
        (theme::ALERT, "daemon down · click to reconnect")
    };
    // The whole footer reconnects — the GUI can't start the daemon itself, so
    // this is the recovery path after the user launches it.
    button(
        row![
            status_dot(c, true),
            text(msg).size(10).font(mono()).color(theme::DIM)
        ]
        .spacing(8)
        .align_y(Vertical::Center),
    )
    .width(Length::Fill)
    .padding(pad2(9.0, 16.0))
    .on_press(Message::Reconnect)
    .style(|_, status| button::Style {
        background: Some(Background::Color(
            if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                theme::HOVER
            } else {
                theme::PANEL
            },
        )),
        ..Default::default()
    })
    .into()
}

// ------------------------------------------------------------- terminal pane

fn terminal_pane(app: &App) -> Element<'_, Message> {
    let head = term_head(app);

    let screen: Element<'_, Message> = match (&app.status, app.model.active.is_some()) {
        (Status::Disconnected(msg), _) => center_note(format!("connection lost: {msg}")),
        (Status::Ended(msg), _) => center_note(format!("session ended: {msg}")),
        (_, false) => center_note("Select a session, or connect a host to start.".into()),
        (_, true) => canvas(TermCanvas {
            frame: app.frame.as_ref(),
            cache: &app.cache,
            metrics: app.metrics,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into(),
    };

    let pane = column![
        head,
        hline(theme::LINE_SOFT),
        container(screen).height(Length::Fill)
    ]
    .height(Length::Fill);
    container(pane)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| panel(theme::SCREEN))
        .into()
}

fn term_head(app: &App) -> Element<'_, Message> {
    let Some((host_id, name)) = &app.model.active else {
        return container(text("no session").size(12).font(mono()).color(theme::DIM))
            .width(Length::Fill)
            .height(Length::Fixed(TERMHEAD_H))
            .padding(pad2(9.0, 14.0))
            .style(|_| panel(theme::HEAD))
            .into();
    };
    let host = app.model.host(*host_id);
    let is_remote = host.is_some_and(Host::is_remote);
    let accent = theme::rail(is_remote);
    let host_label = host.map(Host::label).unwrap_or_default();

    let facts = row![
        text(format!("{} × {}", app.live_cols, app.live_rows))
            .size(11)
            .font(mono())
            .color(theme::MUTED),
    ]
    .align_y(Vertical::Center);

    let line = row![
        text(name.as_str())
            .size(13)
            .font(bold())
            .color(theme::BRIGHT),
        host_chip(host_label, accent),
        spacer(),
        facts,
    ]
    .spacing(10)
    .align_y(Vertical::Center);

    container(line)
        .width(Length::Fill)
        .height(Length::Fixed(TERMHEAD_H))
        .padding(pad2(0.0, 14.0))
        .style(|_| panel(theme::HEAD))
        .into()
}

// -------------------------------------------------------------- status bar

fn status_bar(app: &App) -> Element<'_, Message> {
    let left = match &app.model.active {
        Some((_, name)) => row![
            text(name.as_str())
                .size(11)
                .font(bold())
                .color(theme::MUTED),
            text("attached").size(11).font(mono()).color(theme::DIM),
        ]
        .spacing(6),
        None => row![text("no session").size(11).font(mono()).color(theme::DIM)],
    };

    let right = row![
        hint("+", "new"),
        sep(),
        hint("×", "kill"),
        sep(),
        hint("user@host", "add remote"),
    ]
    .spacing(10)
    .align_y(Vertical::Center);

    container(
        row![left, spacer(), right]
            .align_y(Vertical::Center)
            .spacing(14),
    )
    .width(Length::Fill)
    .height(Length::Fixed(STATUS_H))
    .padding(pad2(0.0, 14.0))
    .style(|_| panel(theme::STATUS))
    .into()
}

// ------------------------------------------------------------------ atoms

fn keycap(c: char) -> Element<'static, Message> {
    container(text(c.to_string()).size(12).font(bold()).color(theme::TEXT))
        .width(Length::Fixed(22.0))
        .height(Length::Fixed(22.0))
        .align_x(Horizontal::Center)
        .align_y(Vertical::Center)
        .style(|_| container::Style {
            background: Some(Background::Color(theme::KEYCAP)),
            border: Border {
                color: theme::KEYCAP_EDGE,
                width: 1.0,
                radius: 5.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn status_dot(color: Color, filled: bool) -> Element<'static, Message> {
    dot(7.0, color, filled)
}
fn session_dot(color: Color, filled: bool) -> Element<'static, Message> {
    dot(8.0, color, filled)
}
fn dot(size: f32, color: Color, filled: bool) -> Element<'static, Message> {
    container(text(""))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(move |_| container::Style {
            background: filled.then_some(Background::Color(color)),
            border: Border {
                color,
                width: if filled { 0.0 } else { 1.5 },
                radius: (size / 2.0).into(),
            },
            ..Default::default()
        })
        .into()
}

fn peers_pill(accent: Color, n: u32) -> Element<'static, Message> {
    container(text(format!("{n}")).size(9).font(bold()).color(theme::VOID))
        .padding(pad2(1.0, 5.0))
        .style(move |_| container::Style {
            background: Some(Background::Color(accent)),
            border: Border {
                radius: 20.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

fn host_chip(label: String, accent: Color) -> Element<'static, Message> {
    container(
        text(label.to_uppercase())
            .size(10)
            .font(mono())
            .color(accent),
    )
    .padding(pad2(2.0, 7.0))
    .style(move |_| container::Style {
        background: Some(Background::Color(theme::tint(accent, 0.12))),
        border: Border {
            color: theme::tint(accent, 0.45),
            width: 1.0,
            radius: 20.0.into(),
        },
        ..Default::default()
    })
    .into()
}

fn label(s: &str) -> Element<'_, Message> {
    container(text(s).size(10).font(mono()).color(theme::DIM))
        .padding(pad(12.0, 16.0, 6.0, 16.0))
        .into()
}

/// A small glyph button (new / kill / disconnect), quiet until hovered.
fn icon_button(glyph: &str, msg: Message, color: Color) -> Element<'static, Message> {
    button(text(glyph.to_string()).size(13).font(mono()).color(color))
        .padding(pad2(1.0, 6.0))
        .on_press(msg)
        .style(move |_, status| button::Style {
            background: matches!(status, button::Status::Hovered | button::Status::Pressed)
                .then_some(Background::Color(theme::HOVER)),
            text_color: color,
            border: Border {
                radius: 5.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

fn hint(kbd: &str, what: &str) -> Element<'static, Message> {
    row![
        container(
            text(kbd.to_string())
                .size(10)
                .font(mono())
                .color(theme::MUTED)
        )
        .padding(pad2(1.0, 5.0))
        .style(|_| container::Style {
            background: Some(Background::Color(theme::SCREEN)),
            border: Border {
                color: theme::LINE,
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        }),
        text(what.to_string())
            .size(11)
            .font(mono())
            .color(theme::DIM),
    ]
    .spacing(5)
    .align_y(Vertical::Center)
    .into()
}

fn sep() -> Element<'static, Message> {
    text("·").size(11).color(theme::LINE).into()
}

fn center_note(msg: String) -> Element<'static, Message> {
    // mouse_area keeps the pane clickable/focusable even with no terminal.
    mouse_area(
        container(text(msg).size(12).font(mono()).color(theme::MUTED))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center),
    )
    .into()
}

// ------------------------------------------------------------------ styles

/// A flat panel fill. Panels are separated by explicit 1px [`vline`]/[`hline`]
/// dividers rather than borders (iced borders are uniform on all four sides,
/// which would box each panel instead of drawing one clean edge).
fn panel(bg: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(bg)),
        ..Default::default()
    }
}

/// A 1px vertical divider that fills its row's height.
fn vline() -> Element<'static, Message> {
    container(text(""))
        .width(Length::Fixed(1.0))
        .height(Length::Fill)
        .style(|_| panel(theme::LINE))
        .into()
}

/// A 1px horizontal divider that fills its column's width.
fn hline(color: Color) -> Element<'static, Message> {
    container(text(""))
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(move |_| panel(color))
        .into()
}

fn session_style(selected: bool, accent: Color, status: button::Status) -> button::Style {
    let bg = if selected {
        Some(Background::Color(theme::tint(accent, 0.16)))
    } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        Some(Background::Color(theme::HOVER))
    } else {
        None
    };
    button::Style {
        background: bg,
        text_color: theme::TEXT,
        border: Border {
            radius: 7.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
