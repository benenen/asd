//! asd-vt test contract (spec §8):
//! replay VT sequence samples and assert key cells; after feeding snapshot_vt
//! back into a fresh terminal, render_snapshot matches (snapshot fidelity,
//! pinning down item 2 of the M0 to-verify checklist).

use asd_vt::{CellWidth, GhosttyVt, Key, KeyEvent, Mods, Rgb, VtBackend};

fn term(cols: u16, rows: u16) -> GhosttyVt {
    GhosttyVt::new(cols, rows, 1000)
}

fn row_text(snap: &asd_vt::RenderSnapshot, row: usize) -> String {
    snap.cells[row]
        .iter()
        .map(|c| c.grapheme.as_str())
        .collect()
}

#[test]
fn plain_text_lands_in_first_row() {
    let mut vt = term(20, 5);
    vt.feed(b"hello");
    let snap = vt.render_snapshot();
    assert_eq!(row_text(&snap, 0), "hello");
    assert_eq!(snap.cols, 20);
    assert_eq!(snap.rows, 5);
    assert_eq!(snap.cursor.position, Some((5, 0)));
    assert!(snap.cursor.visible);
}

#[test]
fn sgr_colors_and_attributes_are_resolved() {
    let mut vt = term(40, 5);
    // 31 = red fg, 1 = bold; 4 = underline; 48;2 = truecolor background
    vt.feed(b"\x1b[1;31mE\x1b[0m \x1b[4mu\x1b[0m \x1b[48;2;10;20;30mB\x1b[0m");
    let snap = vt.render_snapshot();

    let e = &snap.cells[0][0];
    assert_eq!(e.grapheme, "E");
    assert!(e.flags.bold);
    assert_eq!(e.fg, Some(snap.palette[1])); // palette red

    let u = &snap.cells[0][2];
    assert_eq!(u.grapheme, "u");
    assert_ne!(u.flags.underline, asd_vt::UnderlineKind::None);

    let b = &snap.cells[0][4];
    assert_eq!(b.grapheme, "B");
    assert_eq!(
        b.bg,
        Some(Rgb {
            r: 10,
            g: 20,
            b: 30
        })
    );
}

#[test]
fn cjk_wide_chars_occupy_two_cells() {
    let mut vt = term(20, 5);
    vt.feed("你好a".as_bytes());
    let snap = vt.render_snapshot();

    assert_eq!(snap.cells[0][0].grapheme, "你");
    assert_eq!(snap.cells[0][0].width, CellWidth::Wide);
    assert_eq!(snap.cells[0][1].width, CellWidth::SpacerTail);
    assert_eq!(snap.cells[0][2].grapheme, "好");
    assert_eq!(snap.cells[0][2].width, CellWidth::Wide);
    assert_eq!(snap.cells[0][4].grapheme, "a");
    assert_eq!(snap.cells[0][4].width, CellWidth::Narrow);
    // Wide chars take two cells, so the cursor should sit past column 5
    assert_eq!(snap.cursor.position, Some((5, 0)));
}

#[test]
fn cursor_positioning_and_erase() {
    let mut vt = term(20, 5);
    vt.feed(b"XXXXX\x1b[2J\x1b[3;4Hab");
    let snap = vt.render_snapshot();
    // Old content is gone after the 2J clear
    assert_eq!(row_text(&snap, 0).trim(), "");
    // CUP is 1-based: row 3, column 4 → (row=2, col=3)
    assert_eq!(snap.cells[2][3].grapheme, "a");
    assert_eq!(snap.cells[2][4].grapheme, "b");
    assert_eq!(snap.cursor.position, Some((5, 2)));
}

#[test]
fn resize_reflows_and_updates_dims() {
    let mut vt = term(20, 5);
    vt.feed(b"hello world");
    vt.resize(40, 10);
    let snap = vt.render_snapshot();
    assert_eq!((snap.cols, snap.rows), (40, 10));
    assert_eq!(row_text(&snap, 0).trim_end(), "hello world");
}

#[test]
fn scrollback_keeps_terminal_consistent_after_overflow() {
    let mut vt = term(10, 3);
    for i in 0..20 {
        vt.feed(format!("line{i}\r\n").as_bytes());
    }
    let snap = vt.render_snapshot();
    // The viewport keeps only the last few lines (20 lines of output scrolled
    // through a 3-row screen); the last row is the blank line after line19
    assert_eq!(row_text(&snap, 0).trim_end(), "line18");
    assert_eq!(row_text(&snap, 1).trim_end(), "line19");
}

#[test]
fn dirty_flags_are_consumed_by_render_snapshot() {
    let mut vt = term(20, 5);
    vt.feed(b"a");
    let first = vt.render_snapshot();
    assert!(first.row_dirty[0], "written row must be dirty");
    let second = vt.render_snapshot();
    assert!(
        second.row_dirty.iter().all(|d| !d),
        "no writes between snapshots → all rows clean, got {:?}",
        second.row_dirty
    );
}

#[test]
fn da1_query_produces_pty_response() {
    let mut vt = term(20, 5);
    assert!(vt.take_pty_responses().is_empty());
    vt.feed(b"\x1b[c"); // DA1: vim's capability probe at startup
    let resp = vt.take_pty_responses();
    assert!(
        resp.starts_with(b"\x1b["),
        "DA1 must produce a CSI response, got {resp:?}"
    );
    // Once taken, the replies are cleared
    assert!(vt.take_pty_responses().is_empty());
}

// ---- Snapshot fidelity (spec §8 item 2) ----

fn assert_snapshot_fidelity(sample: &[u8], cols: u16, rows: u16) {
    let mut original = term(cols, rows);
    original.feed(sample);
    let dump = original.snapshot_vt();

    let mut replica = term(cols, rows);
    replica.feed(&dump);
    // The dump may contain query-style sequences whose replies need no comparison
    let _ = replica.take_pty_responses();

    let a = original.render_snapshot();
    let b = replica.render_snapshot();

    assert_eq!(a.cells, b.cells, "grid mismatch after snapshot replay");
    assert_eq!(a.cursor.position, b.cursor.position, "cursor position");
    assert_eq!(a.cursor.visible, b.cursor.visible, "cursor visibility");
    assert_eq!(a.palette, b.palette, "palette");
    assert_eq!((a.cols, a.rows), (b.cols, b.rows));
}

#[test]
fn snapshot_fidelity_plain_and_styled() {
    assert_snapshot_fidelity(
        b"\x1b[2J\x1b[H\x1b[1;31mError:\x1b[0m something \x1b[4mbroke\x1b[0m\r\n\
          second line\r\n\x1b[7mreverse\x1b[0m",
        40,
        10,
    );
}

#[test]
fn snapshot_fidelity_cjk() {
    assert_snapshot_fidelity(
        "\x1b[2J\x1b[H中文宽字符测试\r\n混合 mixed 内容\r\n\x1b[32m绿色\x1b[0m".as_bytes(),
        40,
        10,
    );
}

#[test]
fn snapshot_fidelity_vim_like_screen() {
    // vim style: primary screen before alt screen + positioning/line
    // clearing/scrolling region + inverse status line
    let mut sample = Vec::new();
    sample.extend_from_slice(b"\x1b[2J\x1b[H");
    for i in 1..=8 {
        sample.extend_from_slice(format!("{i:3} fn main() {{ /* line {i} */ }}\r\n").as_bytes());
    }
    sample.extend_from_slice(b"\x1b[1;9r"); // DECSTBM scrolling region
    sample.extend_from_slice(b"\x1b[10;1H\x1b[7m-- INSERT --\x1b[0m");
    sample.extend_from_slice(b"\x1b[3;5H"); // cursor parked in the body text
    assert_snapshot_fidelity(&sample, 60, 10);
}

#[test]
fn snapshot_fidelity_after_partial_overwrite() {
    assert_snapshot_fidelity(
        b"aaaaaaaaaa\r\nbbbbbbbbbb\r\n\x1b[1;3Hxx\x1b[2;1H\x1b[2Kreplaced",
        20,
        5,
    );
}

// ---- Key encoding ----

#[test]
fn encode_plain_char() {
    let mut vt = term(10, 3);
    assert_eq!(vt.encode_key(KeyEvent::plain(Key::Char('a'))), b"a");
    assert_eq!(vt.encode_key(KeyEvent::plain(Key::Enter)), b"\r");
    assert_eq!(vt.encode_key(KeyEvent::plain(Key::Escape)), b"\x1b");
    assert_eq!(vt.encode_key(KeyEvent::plain(Key::Backspace)), b"\x7f");
}

#[test]
fn encode_ctrl_c() {
    let mut vt = term(10, 3);
    let ev = KeyEvent {
        key: Key::Char('c'),
        mods: Mods {
            ctrl: true,
            ..Default::default()
        },
        text: None,
    };
    assert_eq!(vt.encode_key(ev), b"\x03");
}

#[test]
fn encode_arrow_respects_cursor_key_mode() {
    let mut vt = term(10, 3);
    // Normal mode: CSI A
    assert_eq!(vt.encode_key(KeyEvent::plain(Key::ArrowUp)), b"\x1b[A");
    // DECCKM application mode (enabled by vim and friends): SS3 A
    vt.feed(b"\x1b[?1h");
    assert_eq!(vt.encode_key(KeyEvent::plain(Key::ArrowUp)), b"\x1bOA");
    // Turning it off restores normal encoding
    vt.feed(b"\x1b[?1l");
    assert_eq!(vt.encode_key(KeyEvent::plain(Key::ArrowUp)), b"\x1b[A");
}

#[test]
fn encode_function_key() {
    let mut vt = term(10, 3);
    assert_eq!(vt.encode_key(KeyEvent::plain(Key::F(1))), b"\x1bOP");
}

// ---- scrollback history (M1) ----

fn line(rows: &[Vec<u8>], i: usize) -> String {
    String::from_utf8_lossy(&rows[i]).trim_end().to_string()
}

#[test]
fn history_len_counts_scrollback_plus_screen() {
    let mut vt = term(20, 4);
    // Nothing fed yet: just the live screen rows.
    assert_eq!(vt.history_len(), 4);
    for i in 0..12 {
        vt.feed(format!("line{i}\r\n").as_bytes());
    }
    // 12 printed lines + trailing blank in a 4-row screen => 9 scrollback + 4.
    assert_eq!(vt.history_len(), 13);
}

#[test]
fn fetch_history_reads_scrolled_off_lines() {
    let mut vt = term(20, 4);
    for i in 0..12 {
        vt.feed(format!("line{i}\r\n").as_bytes());
    }
    let total = vt.history_len() as u32;

    // Whole buffer: row 0 is the oldest line, live screen is at the bottom.
    let all = vt.fetch_history(0, total);
    assert_eq!(all.len(), total as usize);
    assert_eq!(line(&all, 0), "line0");
    assert_eq!(line(&all, 11), "line11");

    // A window in the middle of scrollback.
    let win = vt.fetch_history(2, 3);
    assert_eq!(win.len(), 3);
    assert_eq!(line(&win, 0), "line2");
    assert_eq!(line(&win, 1), "line3");
    assert_eq!(line(&win, 2), "line4");
}

#[test]
fn fetch_history_clamps_to_available_rows() {
    let mut vt = term(20, 4);
    for i in 0..6 {
        vt.feed(format!("row{i}\r\n").as_bytes());
    }
    let total = vt.history_len();
    // Asking for more than remains from `start` returns only what is
    // available (no rows past the end of screen space).
    let rows = vt.fetch_history(total as u32 - 2, 10);
    assert_eq!(rows.len(), 2);
    // Out-of-range start is clamped to the last row, never panics.
    let past = vt.fetch_history(total as u32 + 100, 5);
    assert_eq!(past.len(), 1);
}

#[test]
fn fetch_history_preserves_wide_chars() {
    let mut vt = term(20, 3);
    for i in 0..6 {
        vt.feed(format!("中文{i}\r\n").as_bytes());
    }
    let total = vt.history_len() as u32;
    let all = vt.fetch_history(0, total);
    assert_eq!(line(&all, 0), "中文0");
}

// ---- viewport scroll + selection (render client) ----

#[test]
fn set_scroll_moves_viewport_into_history() {
    let mut vt = term(20, 4);
    for i in 0..20 {
        vt.feed(format!("row{i}\r\n").as_bytes());
    }
    // At the bottom the viewport shows the last lines. 20 printed lines plus a
    // trailing blank in a 4-row screen → the top visible line is row17.
    vt.set_scroll(0);
    let bottom = vt.render_snapshot();
    assert_eq!(row_text(&bottom, 0).trim_end(), "row17");

    // Scroll up: older lines come into view; scrollback is client-local
    // state on this terminal only.
    let sb = vt.scrollback_rows();
    assert!(sb >= 16, "expected scrollback, got {sb}");
    vt.set_scroll(5);
    let scrolled = vt.render_snapshot();
    assert_eq!(row_text(&scrolled, 0).trim_end(), "row12");

    // Back to bottom.
    vt.set_scroll(0);
    let back = vt.render_snapshot();
    assert_eq!(row_text(&back, 0).trim_end(), "row17");
}

#[test]
fn selection_text_extracts_viewport_range() {
    let mut vt = term(20, 4);
    vt.feed(b"hello world\r\n");
    vt.feed(b"second line\r\n");
    vt.set_scroll(0);
    // Select "world" on row 0 (cols 6..=10).
    let sel = asd_vt::Selection {
        start: (6, 0),
        end: (10, 0),
        block: false,
    };
    assert_eq!(vt.selection_text(sel), "world");
}

#[test]
fn selection_text_screen_spans_off_viewport_rows() {
    let mut vt = term(20, 4);
    for i in 0..20 {
        vt.feed(format!("row{i}\r\n").as_bytes());
    }
    // The viewport is at the live bottom, but a screen-space selection reaches
    // rows that scrolled off — coordinates are absolute (0 = oldest line), so
    // it does not depend on where the viewport currently sits.
    vt.set_scroll(0);
    // Screen rows 2..=4 hold row2/row3/row4 (row 0 = the oldest line).
    let text = vt.selection_text_screen((0, 2), (19, 4));
    assert_eq!(text, "row2\nrow3\nrow4");
}

#[test]
fn alt_screen_and_mouse_tracking_reflect_app_state() {
    let mut vt = term(20, 4);
    assert!(!vt.is_alt_screen());
    assert!(!vt.is_mouse_tracking());
    vt.feed(b"\x1b[?1049h"); // enter alt screen
    assert!(vt.is_alt_screen());
    vt.feed(b"\x1b[?1000h"); // enable mouse tracking
    assert!(vt.is_mouse_tracking());
    vt.feed(b"\x1b[?1000l\x1b[?1049l");
    assert!(!vt.is_alt_screen());
    assert!(!vt.is_mouse_tracking());
}

#[test]
fn mouse_modes_reflect_enabled_dec_modes() {
    let mut vt = term(20, 4);
    assert!(vt.mouse_modes().is_empty(), "no mouse by default");

    // Enable normal tracking (1000) + SGR encoding (1006), like most TUIs.
    vt.feed(b"\x1b[?1000h\x1b[?1006h");
    assert_eq!(vt.mouse_modes(), vec![1000, 1006]);

    // Upgrade to button-event tracking (1002) as well.
    vt.feed(b"\x1b[?1002h");
    assert_eq!(vt.mouse_modes(), vec![1000, 1002, 1006]);

    // Turn it all off (shell prompt): back to empty so native selection works.
    vt.feed(b"\x1b[?1000l\x1b[?1002l\x1b[?1006l");
    assert!(vt.mouse_modes().is_empty());
}

// The formatter emits history + screen as one line stream and always trims
// trailing blank lines, which used to mis-scroll the replay whenever the
// screen ended in blanks (content shifted, cursor on the wrong text). The
// two-pass dump (history, then the screen absolutely positioned) must keep
// these exact; scrollback depth is asserted too because the alignment
// depends on every history row landing in scrollback.

fn assert_fidelity_with_scrollback(sample: &[u8], cols: u16, rows: u16) {
    let mut original = term(cols, rows);
    original.feed(sample);
    let dump = original.snapshot_vt();
    let mut replica = term(cols, rows);
    replica.feed(&dump);
    let _ = replica.take_pty_responses();
    assert_eq!(
        original.scrollback_rows(),
        replica.scrollback_rows(),
        "scrollback depth after replay"
    );
    let a = original.render_snapshot();
    let b = replica.render_snapshot();
    assert_eq!(a.cells, b.cells, "grid mismatch after snapshot replay");
    assert_eq!(a.cursor.position, b.cursor.position, "cursor position");
}

#[test]
fn snapshot_fidelity_scrollback_overflow() {
    // 30 lines through a 10-row screen: 21 lines scroll into history and the
    // screen ends in one blank row (the trimmed-newline off-by-one shape).
    let mut sample = Vec::new();
    for i in 1..=30 {
        sample.extend_from_slice(format!("line {i}\r\n").as_bytes());
    }
    assert_fidelity_with_scrollback(&sample, 40, 10);
}

#[test]
fn snapshot_fidelity_overflow_then_clear() {
    // Scrollback overflow, then the app clears and repaints only the top —
    // most of the screen is trailing blanks (a shell prompt after clear).
    let mut sample = Vec::new();
    for i in 1..=30 {
        sample.extend_from_slice(format!("line {i}\r\n").as_bytes());
    }
    sample.extend_from_slice(b"\x1b[2J\x1b[Hfresh top\r\nfresh second\x1b[1;4H");
    assert_fidelity_with_scrollback(&sample, 40, 10);
}

#[test]
fn snapshot_fidelity_alt_screen_after_history() {
    // Shell history on the primary screen, then a vim-like alternate-screen
    // app: the dump must land the alt content on the alt screen with the
    // cursor exact.
    let mut sample = Vec::new();
    for i in 1..=8 {
        sample.extend_from_slice(format!("line {i}\r\n").as_bytes());
    }
    sample.extend_from_slice(b"\x1b[?1049h\x1b[2J\x1b[Halt line 1\r\nalt line 2\x1b[2;3H");
    let mut original = term(20, 4);
    original.feed(&sample);
    let dump = original.snapshot_vt();
    let mut replica = term(20, 4);
    replica.feed(&dump);
    let _ = replica.take_pty_responses();
    assert!(
        replica.is_alt_screen(),
        "replica must end on the alt screen"
    );
    let a = original.render_snapshot();
    let b = replica.render_snapshot();
    assert_eq!(a.cells, b.cells, "grid mismatch after snapshot replay");
    assert_eq!(a.cursor.position, b.cursor.position, "cursor position");
}
