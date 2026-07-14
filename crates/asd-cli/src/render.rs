//! Terminal rendering for the VT-render attach client: turn a
//! [`RenderSnapshot`] into ANSI for the host terminal.
//!
//! The client does not grab the mouse (so the host terminal's own
//! drag-to-select/copy keeps working); scrollback is driven from the keyboard
//! (see `attach.rs`). Selection and clipboard are therefore the terminal's job,
//! not ours.

use asd_vt::{CellSnapshot, CellWidth, CursorShape, RenderSnapshot, Rgb, UnderlineKind};

/// Render a full frame to ANSI (clear + redraw + cursor). A full repaint each
/// frame is simple and correct; dirty-row optimization can come later.
pub fn render_frame(snap: &RenderSnapshot) -> Vec<u8> {
    let mut out = Vec::with_capacity(snap.cols as usize * snap.rows as usize * 4);
    // Hide the cursor during the repaint; re-shown at the end only if it has a
    // real viewport position (see below).
    out.extend_from_slice(b"\x1b[?25l\x1b[H");

    for (y, row) in snap.cells.iter().enumerate() {
        out.extend_from_slice(format!("\x1b[{};1H", y + 1).as_bytes());
        let mut last_sgr: Option<String> = None; // each row starts clean
        out.extend_from_slice(b"\x1b[0m");
        for cell in row {
            if cell.width == CellWidth::SpacerTail {
                // Emitted as part of the preceding wide char.
                continue;
            }
            let sgr = cell_sgr(cell, snap);
            if last_sgr.as_deref() != Some(sgr.as_str()) {
                out.extend_from_slice(b"\x1b[0m");
                out.extend_from_slice(sgr.as_bytes());
                last_sgr = Some(sgr);
            }
            if cell.grapheme.is_empty() {
                out.push(b' ');
            } else {
                out.extend_from_slice(cell.grapheme.as_bytes());
            }
        }
        out.extend_from_slice(b"\x1b[0m\x1b[K");
    }

    // Cursor: only show it when it has a real viewport position. When scrolled
    // back into history the cursor is off-viewport (position is None) — showing
    // it would strand a stray cursor at the last painted cell (bottom-right).
    if snap.cursor.visible
        && let Some((cx, cy)) = snap.cursor.position
    {
        out.extend_from_slice(format!("\x1b[{};{}H", cy + 1, cx + 1).as_bytes());
        out.extend_from_slice(cursor_shape_seq(snap.cursor.shape));
        out.extend_from_slice(b"\x1b[?25h");
    }
    out
}

fn cursor_shape_seq(shape: CursorShape) -> &'static [u8] {
    match shape {
        CursorShape::Bar => b"\x1b[5 q",
        CursorShape::Underline => b"\x1b[3 q",
        // Hollow has no standard DECSCUSR; render as a block.
        CursorShape::Block | CursorShape::BlockHollow => b"\x1b[1 q",
    }
}

/// Build the SGR parameter sequence for one cell (colors + attributes).
fn cell_sgr(cell: &CellSnapshot, snap: &RenderSnapshot) -> String {
    let mut s = String::from("\x1b[");
    let push = |code: &str, s: &mut String| {
        if !s.ends_with('[') {
            s.push(';');
        }
        s.push_str(code);
    };
    let f = &cell.flags;
    if f.bold {
        push("1", &mut s);
    }
    if f.faint {
        push("2", &mut s);
    }
    if f.italic {
        push("3", &mut s);
    }
    if f.underline != UnderlineKind::None {
        push("4", &mut s);
    }
    if f.blink {
        push("5", &mut s);
    }
    if f.inverse {
        push("7", &mut s);
    }
    if f.strikethrough {
        push("9", &mut s);
    }
    let fg = cell.fg.unwrap_or(snap.foreground);
    let bg = cell.bg.unwrap_or(snap.background);
    push(&rgb_sgr(38, fg), &mut s);
    push(&rgb_sgr(48, bg), &mut s);
    s.push('m');
    s
}

fn rgb_sgr(base: u8, c: Rgb) -> String {
    format!("{base};2;{};{};{}", c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asd_vt::{CursorSnapshot, RenderSnapshot};

    fn blank(cols: u16, rows: u16, cursor: CursorSnapshot) -> RenderSnapshot {
        RenderSnapshot {
            cols,
            rows,
            cells: vec![vec![CellSnapshot::default(); cols as usize]; rows as usize],
            row_dirty: vec![true; rows as usize],
            cursor,
            palette: [Rgb::default(); 256],
            foreground: Rgb {
                r: 200,
                g: 200,
                b: 200,
            },
            background: Rgb::default(),
        }
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn cursor_shown_only_with_a_viewport_position() {
        // Live view: cursor visible with a position → it is shown.
        let live = blank(
            10,
            3,
            CursorSnapshot {
                visible: true,
                position: Some((2, 1)),
                ..CursorSnapshot::default()
            },
        );
        let out = render_frame(&live);
        assert!(contains(&out, b"\x1b[?25h"), "live cursor should be shown");
        assert!(contains(&out, b"\x1b[2;3H"), "cursor should be positioned");

        // Scrolled back: visible but no position → cursor must stay hidden
        // (no residual cursor at bottom-right).
        let scrolled = blank(
            10,
            3,
            CursorSnapshot {
                visible: true,
                position: None,
                ..CursorSnapshot::default()
            },
        );
        let out = render_frame(&scrolled);
        assert!(
            !contains(&out, b"\x1b[?25h"),
            "off-viewport cursor must not be re-shown"
        );
        assert!(contains(&out, b"\x1b[?25l"), "cursor stays hidden");
    }
}
