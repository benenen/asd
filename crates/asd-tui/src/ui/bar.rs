//! Full-width keybinding, server clock, and daemon status bar.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use super::{ACCENT, ALERT, DIM, MUTED, OK, RULE, str_width, truncate};
use crate::App;
use crate::keymap::{KeyAction, KeyHint};

pub(super) fn draw(buf: &mut Buffer, area: Rect, app: &App) {
    if area.height == 0 || area.width < 8 {
        return;
    }
    for x in area.left()..area.right() {
        if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(x, area.top())) {
            cell.set_style(Style::new().bg(RULE));
        }
    }
    let (status, status_style) = status(app);
    draw_text(
        buf,
        area,
        &app.keymap.current_hint(),
        &server_time_at(app.now_ms),
        &status,
        status_style,
    );
}

fn status(app: &App) -> (String, Style) {
    if let Some(notice) = &app.notice {
        return (notice.clone(), Style::new().fg(ALERT).bg(RULE));
    }
    if app.daemon_up {
        let count = app
            .sessions
            .iter()
            .filter(|session| app.self_session.as_deref() != Some(&session.name))
            .count();
        return (
            session_status(count, app.scroll),
            Style::new().fg(OK).bg(RULE),
        );
    }
    let reconnect = app
        .keymap
        .invocation_hint(KeyAction::Reconnect)
        .unwrap_or_else(|| "reconnect unbound".to_string());
    (
        format!("daemon down — {reconnect}"),
        Style::new().fg(ALERT).bg(RULE),
    )
}

fn draw_text(
    buf: &mut Buffer,
    area: Rect,
    hint: &KeyHint,
    server_time: &str,
    status: &str,
    status_style: Style,
) {
    let status = truncate(status, (area.width / 2) as usize);
    let x = area.right().saturating_sub(str_width(&status) as u16 + 1);
    buf.set_string(x, area.top(), status, status_style);
    let left_width = x.saturating_sub(area.left() + 2) as usize;
    draw_left(buf, area, hint, server_time, left_width);
}

fn draw_left(buf: &mut Buffer, area: Rect, hint: &KeyHint, server_time: &str, max_width: usize) {
    let style = if hint.prefix_active {
        Style::new().fg(ACCENT).bg(RULE)
    } else {
        Style::new().fg(DIM).bg(RULE)
    };
    let with_time = format!("{}  {server_time}", hint.text);
    let line = if str_width(&with_time) <= max_width {
        with_time
    } else {
        truncate(&hint.text, max_width)
    };
    buf.set_string(area.left() + 1, area.top(), &line, style);
}

fn server_time_at(timestamp_ms: u64) -> String {
    let timestamp_ms = i64::try_from(timestamp_ms).unwrap_or(i64::MAX);
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(timestamp_ms)
        .map(|time| format_server_time(time.with_timezone(&chrono::Local)))
        .unwrap_or_default()
}

fn format_server_time(time: chrono::DateTime<chrono::Local>) -> String {
    time.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn session_status(count: usize, scroll: usize) -> String {
    if scroll > 0 {
        format!("[+{scroll}] ● {count} sessions")
    } else {
        format!("● {count} sessions")
    }
}

/// Binary units the way `free -h` writes them: one decimal below ten, none
/// above. The bar is short on columns, so nothing is padded.
fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [(u64, char); 3] = [(1 << 30, 'G'), (1 << 20, 'M'), (1 << 10, 'K')];
    for (scale, suffix) in UNITS {
        if bytes >= scale {
            let value = bytes as f64 / scale as f64;
            return if value < 10.0 {
                format!("{value:.1}{suffix}")
            } else {
                format!("{}{suffix}", value.round() as u64)
            };
        }
    }
    format!("{bytes}B")
}

/// A whole percent, clamped. The sampler already clamps; this is the second
/// belt, because printing "103%" reads like a bug rather than a busy host.
fn fmt_pct(pct: u8) -> String {
    format!("{}%", pct.min(100))
}

/// Green until the host is working, amber while it is, red once it is out of
/// room. Only CPU and memory get this: there is no throughput that is "bad",
/// so colouring the network would raise an alarm that means nothing.
fn load_color(pct: u8) -> Color {
    if pct >= 90 {
        ALERT
    } else if pct >= 70 {
        ACCENT
    } else {
        OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Keymap;
    use chrono::{Local, TimeZone};
    use ratatui::layout::Position;

    #[test]
    fn session_status_prefixes_scroll_offset() {
        assert_eq!(session_status(3, 0), "● 3 sessions");
        assert_eq!(session_status(3, 12), "[+12] ● 3 sessions");
        assert_eq!(session_status(0, 0), "● 0 sessions");
    }

    #[test]
    fn bottom_bar_places_the_full_server_time_after_keybinds() {
        let time = Local
            .with_ymd_and_hms(2026, 8, 14, 9, 5, 7)
            .single()
            .expect("the daytime fixture is unambiguous");
        let shown = format_server_time(time);
        assert_eq!(shown, "2026-08-14 09:05:07");
        let area = Rect::new(0, 0, 80, 1);
        let mut buf = Buffer::empty(area);
        draw_text(
            &mut buf,
            area,
            &Keymap::default().current_hint(),
            &shown,
            "● 3 sessions",
            Style::default(),
        );
        let line = (0..area.width)
            .map(|x| buf.cell(Position::new(x, 0)).unwrap().symbol())
            .collect::<String>();
        assert!(
            line.starts_with(" Keybinds: Ctrl+A  2026-08-14 09:05:07"),
            "bar: {line}"
        );
        assert!(line.contains("● 3 sessions"), "bar: {line}");
    }

    #[test]
    fn narrow_bottom_bar_keeps_daemon_status_before_the_clock() {
        let area = Rect::new(0, 0, 42, 1);
        let mut buf = Buffer::empty(area);
        draw_text(
            &mut buf,
            area,
            &Keymap::default().current_hint(),
            "2026-08-14 09:05:07",
            "● 3 sessions",
            Style::default(),
        );
        let line = (0..area.width)
            .map(|x| buf.cell(Position::new(x, 0)).unwrap().symbol())
            .collect::<String>();
        assert!(line.contains("Keybinds: Ctrl+A"), "bar: {line}");
        assert!(line.contains("● 3 sessions"), "bar: {line}");
        assert!(!line.contains("2026-08-14"), "bar: {line}");
    }

    #[test]
    fn bytes_read_the_way_free_h_writes_them() {
        // One decimal below ten, none above, so a value never jitters between
        // widths as it crosses a round number.
        assert_eq!(fmt_bytes(6_549_123_456), "6.1G");
        assert_eq!(fmt_bytes(33_285_996_544), "31G");
        assert_eq!(fmt_bytes(1_258_291), "1.2M");
        assert_eq!(fmt_bytes(348_160), "340K");
        // Below a kibibyte there is no unit worth scaling to.
        assert_eq!(fmt_bytes(512), "512B");
        assert_eq!(fmt_bytes(0), "0B");
    }

    #[test]
    fn a_percent_is_printed_whole_and_never_over_a_hundred() {
        assert_eq!(fmt_pct(0), "0%");
        assert_eq!(fmt_pct(12), "12%");
        assert_eq!(fmt_pct(100), "100%");
        // The sampler clamps, but a value that got past it should read as a
        // busy host rather than a number that looks like a bug.
        assert_eq!(fmt_pct(103), "100%");
    }

    #[test]
    fn load_colour_escalates_at_the_documented_thresholds() {
        assert_eq!(load_color(0), OK);
        assert_eq!(load_color(69), OK);
        assert_eq!(load_color(70), ACCENT);
        assert_eq!(load_color(89), ACCENT);
        assert_eq!(load_color(90), ALERT);
        assert_eq!(load_color(100), ALERT);
    }
}
