//! Session sidebar rendering and pointer hit testing.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};

use super::{ACCENT, ALERT, DIM, MUTED, ROW_TEXT_X, RULE, SELECT_BG, TEXT, truncate};
use crate::App;
use crate::keymap::KeyAction;

pub(super) fn draw(buf: &mut Buffer, area: Rect, app: &App) {
    if area.width < 4 || area.height < 3 {
        return;
    }
    for y in area.top()..area.bottom() {
        buf.set_string(area.right() - 1, y, "│", Style::new().fg(RULE));
    }
    let offset = app.sidebar_offset();
    let mut y = area.top();
    for (i, session) in app.sessions.iter().enumerate().skip(offset) {
        if y + 1 >= area.bottom() {
            break;
        }
        draw_row(buf, area, app, i, session, y);
        y += 2;
    }
}

fn draw_row(
    buf: &mut Buffer,
    area: Rect,
    app: &App,
    index: usize,
    session: &asd_proto::SessionInfo,
    y: u16,
) {
    let selected = app.active.as_deref() == Some(&session.name);
    let row_bg = if selected {
        Style::new().bg(SELECT_BG)
    } else {
        Style::new()
    };
    for line in 0..2 {
        for x in area.left()..area.right() - 1 {
            if let Some(cell) = buf.cell_mut(Position::new(x, y + line)) {
                cell.set_style(row_bg);
            }
        }
    }
    draw_title(buf, area, app, session, index, y);
    draw_detail(buf, area, app, session, y);
    if selected {
        for line in 0..2 {
            buf.set_string(area.left(), y + line, "│", row_bg.fg(ACCENT));
            buf.set_string(area.right() - 1, y + line, "│", Style::new().fg(ACCENT));
        }
    }
}

/// Whether this row should read as "waiting on you".
///
/// Checked before `running`, which it can coexist with: an agent that has just
/// drawn a permission prompt produced output a moment ago, so activity still
/// calls it running while the screen says it has stopped and is asking.
fn blocked(session: &asd_proto::SessionInfo) -> bool {
    session.state == asd_proto::AgentState::Blocked
}

fn draw_title(
    buf: &mut Buffer,
    area: Rect,
    app: &App,
    session: &asd_proto::SessionInfo,
    index: usize,
    y: u16,
) {
    let selected = app.active.as_deref() == Some(&session.name);
    let is_self = app.self_session.as_deref() == Some(&session.name);
    let row_bg = if selected {
        Style::new().bg(SELECT_BG)
    } else {
        Style::new()
    };
    let ordinal = index + 1;
    let ordinal_style = if app.active.as_deref() == Some(&session.name) {
        row_bg.fg(ACCENT)
    } else if u16::try_from(ordinal)
        .ok()
        .is_some_and(|ordinal| app.keymap.is_bound(KeyAction::JumpTo(ordinal)))
    {
        row_bg.fg(MUTED)
    } else {
        row_bg.fg(DIM)
    };
    buf.set_string(area.left() + 1, y, format!("{ordinal:<2}"), ordinal_style);
    let name_style = if is_self {
        row_bg.fg(DIM)
    } else if blocked(session) {
        row_bg.fg(ALERT).add_modifier(Modifier::BOLD)
    } else if session.running {
        row_bg.fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        row_bg.fg(TEXT).add_modifier(Modifier::BOLD)
    };
    let name = truncate(
        &session.name,
        (area.width - 1).saturating_sub(ROW_TEXT_X + 3) as usize,
    );
    buf.set_string(area.left() + ROW_TEXT_X, y, &name, name_style);
    buf.set_string(area.right() - 3, y, "x", row_bg.fg(DIM));
}

fn draw_detail(buf: &mut Buffer, area: Rect, app: &App, session: &asd_proto::SessionInfo, y: u16) {
    let is_self = app.self_session.as_deref() == Some(&session.name);
    let row_bg = if app.active.as_deref() == Some(&session.name) {
        Style::new().bg(SELECT_BG)
    } else {
        Style::new()
    };
    let age = short_age(session.created_ms, app.now_ms);
    let cmd_w = (area.width - 1) as usize;
    let cmd_w = cmd_w.saturating_sub(ROW_TEXT_X as usize + age.len() + 2);
    // Most deliberate first: what the session said about itself with `asd
    // status`, then the title its program set, then the command it is running.
    let label = if !session.status_line.trim().is_empty() {
        session.status_line.trim().to_string()
    } else if session.title.trim().is_empty() {
        short_cmd(&session.command)
    } else {
        session.title.trim().to_string()
    };
    // A marker as well as a colour: in a sidebar of twenty rows, colour alone
    // is easy to miss, and it is gone entirely for a colour-blind reader.
    let label = if blocked(session) {
        format!("! {label}")
    } else {
        label
    };
    let cmd = truncate(&label, cmd_w);
    let cmd_fg = if blocked(session) {
        ALERT
    } else if session.running && !is_self {
        ACCENT
    } else {
        MUTED
    };
    buf.set_string(area.left() + ROW_TEXT_X, y + 1, &cmd, row_bg.fg(cmd_fg));
    buf.set_string(
        area.right() - 2 - age.len() as u16,
        y + 1,
        &age,
        row_bg.fg(DIM),
    );
}

pub(super) fn hit(
    side: Rect,
    sessions: usize,
    offset: usize,
    col: u16,
    row: u16,
) -> Option<(usize, bool)> {
    if side.width == 0 || col >= side.right().saturating_sub(1) || row >= side.bottom() {
        return None;
    }
    let index = offset + ((row.checked_sub(side.top())?) / 2) as usize;
    if index >= sessions {
        return None;
    }
    let on_kill =
        row.saturating_sub(side.top()).is_multiple_of(2) && col >= side.right().saturating_sub(4);
    Some((index, on_kill))
}

fn short_cmd(command: &str) -> String {
    let command = command.trim();
    if command.is_empty() {
        return String::new();
    }
    if !command.contains(char::is_whitespace) && command.contains('/') {
        command.rsplit('/').next().unwrap_or(command).to_string()
    } else {
        command.to_string()
    }
}

fn short_age(created_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(created_ms) / 1000;
    if secs < 60 {
        "now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::str_width;

    #[test]
    fn long_title_never_overlaps_the_age_box() {
        let area_w: u16 = 28;
        let age = "13h";
        let inner_w = (area_w - 1) as usize;
        let budget = inner_w.saturating_sub(ROW_TEXT_X as usize + str_width(age) + 2);
        let title = "远端一个非常非常长的会话标题名字啊啊啊啊";
        let shown = truncate(title, budget);
        let text_end = ROW_TEXT_X as usize + str_width(&shown);
        let age_start = area_w as usize - 2 - str_width(age);
        assert!(text_end < age_start);
        assert!(shown.ends_with('…'));
        assert!(str_width(&shown) <= budget);
    }

    fn info(state: asd_proto::AgentState, running: bool) -> asd_proto::SessionInfo {
        asd_proto::SessionInfo {
            name: "web".into(),
            command: "claude".into(),
            title: "Refactor auth".into(),
            status_line: String::new(),
            created_ms: 0,
            idle_ms: 0,
            running,
            state,
            attached_clients: 0,
            pid: 1,
            cols: 80,
            rows: 24,
        }
    }

    #[test]
    fn blocked_outranks_running_on_a_row() {
        use asd_proto::AgentState;

        // The screen that draws a permission prompt is output like any other,
        // so a session can be blocked and still count as running. The row has
        // to say the part a person can act on.
        assert!(blocked(&info(AgentState::Blocked, true)));
        assert!(blocked(&info(AgentState::Blocked, false)));
        assert!(!blocked(&info(AgentState::Working, true)));
        assert!(!blocked(&info(AgentState::Idle, false)));
        // An ordinary shell is never marked: nothing recognized it.
        assert!(!blocked(&info(AgentState::Unknown, false)));
    }

    #[test]
    fn short_cmd_basenames_paths_but_keeps_args() {
        assert_eq!(short_cmd("/usr/bin/bash"), "bash");
        assert_eq!(short_cmd("journalctl -f"), "journalctl -f");
        assert_eq!(short_cmd(""), "");
    }

    #[test]
    fn short_age_buckets() {
        let minute = 60_000;
        assert_eq!(short_age(0, 30 * 1000), "now");
        assert_eq!(short_age(0, 5 * minute), "5m");
        assert_eq!(short_age(0, 120 * minute), "2h");
        assert_eq!(short_age(1_000, 0), "now");
    }
}
