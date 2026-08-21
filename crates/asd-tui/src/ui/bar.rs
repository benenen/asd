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
        app.metrics,
    );
}

/// One run of the left-hand group: a dim icon and a coloured value. The icon
/// carries no colour of its own so the eye lands on the number.
struct Segment {
    icon: &'static str,
    value: String,
    value_style: Style,
}

impl Segment {
    fn width(&self) -> usize {
        // icon + one space + value
        str_width(self.icon) + 1 + str_width(&self.value)
    }
}

/// The metric segments in display order. Dropping from the end of this vector
/// is what produces the documented narrow-terminal order: network, then
/// memory, then CPU, then the clock.
fn segments(server_time: &str, metrics: Option<asd_proto::HostSample>) -> Vec<Segment> {
    let mut out = vec![Segment {
        icon: "🕐",
        value: server_time.to_string(),
        value_style: Style::new().fg(MUTED).bg(RULE),
    }];
    let Some(m) = metrics else {
        return out;
    };
    out.push(Segment {
        icon: "💻",
        value: fmt_pct(m.cpu_pct),
        value_style: Style::new().fg(load_color(m.cpu_pct)).bg(RULE),
    });
    let mem_pct = if m.mem_total_bytes == 0 {
        0
    } else {
        ((m.mem_used_bytes as f64 / m.mem_total_bytes as f64) * 100.0).round() as u8
    };
    out.push(Segment {
        icon: "🧠",
        value: format!(
            "{}/{}",
            fmt_bytes(m.mem_used_bytes),
            fmt_bytes(m.mem_total_bytes)
        ),
        value_style: Style::new().fg(load_color(mem_pct)).bg(RULE),
    });
    out.push(Segment {
        icon: "🌐",
        value: format!("↓{} ↑{}", fmt_bytes(m.net_rx_bps), fmt_bytes(m.net_tx_bps)),
        value_style: Style::new().fg(MUTED).bg(RULE),
    });
    out
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
    metrics: Option<asd_proto::HostSample>,
) {
    let status = truncate(status, (area.width / 2) as usize);
    let x = area.right().saturating_sub(str_width(&status) as u16 + 1);
    buf.set_string(x, area.top(), status, status_style);
    let left_width = x.saturating_sub(area.left() + 2) as usize;
    draw_left(buf, area, hint, server_time, metrics, left_width);
}

fn draw_left(
    buf: &mut Buffer,
    area: Rect,
    hint: &KeyHint,
    server_time: &str,
    metrics: Option<asd_proto::HostSample>,
    max_width: usize,
) {
    let hint_style = if hint.prefix_active {
        Style::new().fg(ACCENT).bg(RULE)
    } else {
        Style::new().fg(DIM).bg(RULE)
    };
    let icon_style = Style::new().fg(DIM).bg(RULE);

    // Two spaces separate the keybind hint from the first segment and each
    // segment from the next, matching the gap the clock has always had.
    const GAP: usize = 2;
    let mut segs = segments(server_time, metrics);
    let hint_width = str_width(&hint.text);
    while !segs.is_empty()
        && hint_width + segs.iter().map(|s| GAP + s.width()).sum::<usize>() > max_width
    {
        segs.pop();
    }

    let mut x = area.left() + 1;
    if segs.is_empty() {
        // Nothing else fits: the hint alone, truncated, as it has always been.
        buf.set_string(x, area.top(), truncate(&hint.text, max_width), hint_style);
        return;
    }
    buf.set_string(x, area.top(), &hint.text, hint_style);
    x += hint_width as u16;
    for seg in &segs {
        x += GAP as u16;
        buf.set_string(x, area.top(), seg.icon, icon_style);
        let icon_width = str_width(seg.icon) as u16;
        // Most of these icons are double-width emoji. `set_string` writes the
        // glyph into the first cell and calls `Cell::reset()` on the
        // continuation cell(s); a reset cell's background is `Color::Reset`,
        // not the stripe's `RULE`, which punches a hole in the bar. Re-apply
        // the stripe background across the icon's full width to close it.
        for col in 0..icon_width {
            if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(x + col, area.top())) {
                cell.set_style(Style::new().bg(RULE));
            }
        }
        x += icon_width + 1;
        buf.set_string(x, area.top(), &seg.value, seg.value_style);
        x += str_width(&seg.value) as u16;
    }
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

/// Binary units the way `free -h` writes them: one decimal below ten, none at
/// or above it. The bar is short on columns, so nothing is padded.
///
/// The unit is chosen from the value as it will be *rendered*, not from the
/// raw ratio. Picking the unit first and rounding afterwards is what produces
/// "1024K" for one byte under a mebibyte, and "10.0G" — a decimal at ten — for
/// anything from 9.95 GiB up.
fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [char; 3] = ['K', 'M', 'G'];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    // Promote while the whole-number form would read 1024 or more.
    while unit + 1 < UNITS.len() && value >= 1023.5 {
        value /= 1024.0;
        unit += 1;
    }
    let suffix = UNITS[unit];
    if value < 9.95 {
        format!("{value:.1}{suffix}")
    } else {
        format!("{}{suffix}", value.round() as u64)
    }
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
        let hint = Keymap::default().current_hint();
        let area = Rect::new(0, 0, 80, 1);
        let mut buf = Buffer::empty(area);
        draw_text(
            &mut buf,
            area,
            &hint,
            &shown,
            "● 3 sessions",
            Style::default(),
            None,
        );
        let line = (0..area.width)
            .map(|x| buf.cell(Position::new(x, 0)).unwrap().symbol())
            .collect::<String>();
        // Built from the same parts draw_left assembles: the hint, the
        // two-space GAP, the clock icon, then its value. The clock icon is
        // a double-width glyph -- ratatui writes it into one cell and
        // resets the cell after it, and a reset cell's symbol() reads back
        // as a literal space rather than empty, so the icon is followed by
        // *two* space characters here: that wide-glyph continuation cell,
        // then the single explicit separator space every segment has
        // between its icon and its value. Confirmed by inspecting the
        // buffer directly, not assumed.
        let expected = format!(" {}  🕐  {shown}", hint.text);
        assert!(line.starts_with(&expected), "bar: {line}");
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
            None,
        );
        let line = (0..area.width)
            .map(|x| buf.cell(Position::new(x, 0)).unwrap().symbol())
            .collect::<String>();
        assert!(line.contains("Keybinds: Ctrl+A"), "bar: {line}");
        assert!(line.contains("● 3 sessions"), "bar: {line}");
        assert!(!line.contains("2026-08-14"), "bar: {line}");
    }

    fn sample() -> asd_proto::HostSample {
        asd_proto::HostSample {
            cpu_pct: 12,
            mem_used_bytes: 6_549_123_456,
            mem_total_bytes: 33_285_996_544,
            net_rx_bps: 1_258_291,
            net_tx_bps: 348_160,
            sampled_age_ms: 740,
        }
    }

    fn rendered(width: u16, metrics: Option<asd_proto::HostSample>) -> String {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        draw_text(
            &mut buf,
            area,
            &Keymap::default().current_hint(),
            "2026-08-14 09:05:07",
            "● 3 sessions",
            Style::default(),
            metrics,
        );
        (0..area.width)
            .map(|x| buf.cell(Position::new(x, 0)).unwrap().symbol())
            .collect()
    }

    #[test]
    fn a_wide_bar_shows_every_segment() {
        let line = rendered(160, Some(sample()));
        assert!(line.contains("Keybinds: Ctrl+A"), "bar: {line}");
        assert!(line.contains("2026-08-14 09:05:07"), "bar: {line}");
        assert!(line.contains("12%"), "bar: {line}");
        assert!(line.contains("6.1G/31G"), "bar: {line}");
        assert!(line.contains("↓1.2M ↑340K"), "bar: {line}");
        assert!(line.contains("● 3 sessions"), "bar: {line}");
    }

    #[test]
    fn every_icon_keeps_the_stripe_background_across_its_full_width() {
        // Every segment icon here (🕐, 💻, 🧠, 🌐) is a two-column glyph.
        // `Buffer::set_stringn` writes the glyph into the first cell and
        // resets the continuation cell, and a reset cell's background is
        // `Color::Reset`, not the stripe's `RULE`. Reading back `symbol()`
        // cannot see this -- a reset cell's symbol reads as a plain space,
        // same as an intentional gap -- so this asserts on style/bg instead.
        let area = Rect::new(0, 0, 160, 1);
        let mut buf = Buffer::empty(area);
        draw_text(
            &mut buf,
            area,
            &Keymap::default().current_hint(),
            "2026-08-14 09:05:07",
            "● 3 sessions",
            Style::default(),
            Some(sample()),
        );
        let mut checked = 0;
        for x in 0..area.width {
            let cell = buf.cell(Position::new(x, 0)).expect("cell in bounds");
            if ["🕐", "💻", "🧠", "🌐"].contains(&cell.symbol()) {
                assert_eq!(
                    cell.bg, RULE,
                    "icon cell at {x} lost the stripe background: {cell:?}"
                );
                let notch = buf
                    .cell(Position::new(x + 1, 0))
                    .expect("continuation cell in bounds");
                assert_eq!(
                    notch.bg,
                    RULE,
                    "reset continuation cell at {} notched the stripe: {notch:?}",
                    x + 1
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 4, "expected all four segment icons on a wide bar");
    }

    #[test]
    fn segments_drop_right_to_left_as_the_bar_narrows() {
        // Network goes first, then memory, then CPU, then the clock. The
        // keybind hint outlives them all because it is the only actionable
        // thing on the bar, and the clock outlives the new segments so a narrow
        // terminal keeps behaving the way it did before they existed.
        let mut seen_without = Vec::new();
        for width in [160u16, 120, 100, 84, 64, 42] {
            let line = rendered(width, Some(sample()));
            seen_without.push((
                width,
                line.contains("↓1.2M"),
                line.contains("6.1G/31G"),
                line.contains("12%"),
                line.contains("2026-08-14"),
                line.contains("Keybinds"),
            ));
        }
        // Whatever the exact widths, a segment never comes back once dropped,
        // and the keybind hint is present at every width.
        let mut net = true;
        let mut mem = true;
        let mut cpu = true;
        let mut clock = true;
        for (width, has_net, has_mem, has_cpu, has_clock, has_keys) in seen_without {
            assert!(has_keys, "keybinds vanished at {width}");
            assert!(!has_net || net, "network came back at {width}");
            assert!(!has_mem || mem, "memory came back at {width}");
            assert!(!has_cpu || cpu, "cpu came back at {width}");
            assert!(!has_clock || clock, "clock came back at {width}");
            // Ordering: a segment cannot outlive one to its right.
            assert!(
                has_net <= has_mem,
                "memory dropped before network at {width}"
            );
            assert!(has_mem <= has_cpu, "cpu dropped before memory at {width}");
            assert!(has_cpu <= has_clock, "clock dropped before cpu at {width}");
            net = has_net;
            mem = has_mem;
            cpu = has_cpu;
            clock = has_clock;
        }
    }

    #[test]
    fn without_a_sample_only_the_clock_is_shown() {
        // No reading yet: the clock is the whole left-hand group beyond the
        // hint. It keeps its icon -- the icon marks the segment, not the
        // presence of data -- so this line differs from the pre-feature bar by
        // exactly that icon and nothing else.
        let line = rendered(160, None);
        assert!(line.contains("🕐"), "bar: {line}");
        assert!(line.contains("2026-08-14 09:05:07"), "bar: {line}");
        assert!(!line.contains('%'), "bar: {line}");
        assert!(!line.contains("💻"), "bar: {line}");
        assert!(!line.contains("🌐"), "bar: {line}");
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
        assert_eq!(fmt_bytes(1023), "1023B");
    }

    #[test]
    fn a_size_that_rounds_up_promotes_instead_of_reading_1024() {
        // One byte under each boundary. Choosing the unit from the raw ratio
        // and rounding afterwards renders these "1024K" and "1024M", which is
        // not a unit anyone writes.
        assert_eq!(fmt_bytes((1 << 20) - 1), "1.0M");
        assert_eq!(fmt_bytes((1 << 30) - 1), "1.0G");
        // Exactly on the boundary, for the other side of the same fence.
        assert_eq!(fmt_bytes(1 << 20), "1.0M");
        assert_eq!(fmt_bytes(1 << 30), "1.0G");
    }

    #[test]
    fn a_size_that_rounds_up_to_ten_drops_its_decimal() {
        // 9.95 GiB and up round to ten. Keeping the decimal there widens the
        // field from "9.9G" to "10.0G", which is the jitter the one-decimal
        // rule exists to avoid.
        assert_eq!(fmt_bytes(10_684_795_973), "10G");
        assert_eq!(fmt_bytes(10_630_000_000), "9.9G");
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
