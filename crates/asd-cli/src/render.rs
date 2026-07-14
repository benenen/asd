//! Terminal rendering for the VT-render attach client: turn a
//! [`RenderSnapshot`] into ANSI, parse SGR mouse reports, and write the
//! system clipboard via OSC 52.

use asd_vt::{CellSnapshot, CellWidth, CursorShape, RenderSnapshot, Rgb, UnderlineKind};

/// A text selection over the viewport, in cell coordinates (inclusive ends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub start: (u16, u16),
    pub end: (u16, u16),
}

impl Selection {
    /// Normalize to row-major order (start ≤ end).
    fn ordered(self) -> ((u16, u16), (u16, u16)) {
        let a = self.start;
        let b = self.end;
        if (a.1, a.0) <= (b.1, b.0) {
            (a, b)
        } else {
            (b, a)
        }
    }

    fn contains(self, x: u16, y: u16) -> bool {
        let (a, b) = self.ordered();
        let after_start = y > a.1 || (y == a.1 && x >= a.0);
        let before_end = y < b.1 || (y == b.1 && x <= b.0);
        after_start && before_end
    }
}

/// Render a full frame to ANSI (clear + redraw + cursor). A full repaint each
/// frame is simple and correct; dirty-row optimization can come later.
pub fn render_frame(snap: &RenderSnapshot, sel: Option<Selection>) -> Vec<u8> {
    let mut out = Vec::with_capacity(snap.cols as usize * snap.rows as usize * 4);
    // Hide the cursor during the repaint to avoid flicker; restore at the end.
    out.extend_from_slice(b"\x1b[?25l\x1b[H");

    for (y, row) in snap.cells.iter().enumerate() {
        out.extend_from_slice(format!("\x1b[{};1H", y + 1).as_bytes());
        let mut last_sgr: Option<String> = None; // each row starts clean
        out.extend_from_slice(b"\x1b[0m");
        let mut x = 0u16;
        for cell in row {
            if cell.width == CellWidth::SpacerTail {
                // Emitted as part of the preceding wide char.
                continue;
            }
            let selected = sel.is_some_and(|s| s.contains(x, y as u16));
            let sgr = cell_sgr(cell, snap, selected);
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
            x += if cell.width == CellWidth::Wide { 2 } else { 1 };
        }
        out.extend_from_slice(b"\x1b[0m\x1b[K");
    }

    // Cursor.
    if snap.cursor.visible {
        if let Some((cx, cy)) = snap.cursor.position {
            out.extend_from_slice(format!("\x1b[{};{}H", cy + 1, cx + 1).as_bytes());
        }
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

/// Build the SGR parameter sequence for one cell (colors + attributes, with
/// selection shown as reverse video).
fn cell_sgr(cell: &CellSnapshot, snap: &RenderSnapshot, selected: bool) -> String {
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
    // Selection inverts; if the cell is already inverse, cancel out.
    if f.inverse ^ selected {
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

// ---- SGR mouse reports ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Press,
    Drag,
    Release,
    WheelUp,
    WheelDown,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    /// 0-based column and row.
    pub x: u16,
    pub y: u16,
    pub kind: MouseKind,
}

/// Parse every SGR mouse report (`ESC [ < b ; x ; y M|m`) in a chunk.
pub fn parse_mouse(chunk: &[u8]) -> Vec<MouseEvent> {
    let mut events = Vec::new();
    let mut i = 0;
    while i + 3 < chunk.len() {
        if &chunk[i..i + 3] == b"\x1b[<" {
            let body_start = i + 3;
            let mut j = body_start;
            while j < chunk.len() && chunk[j] != b'M' && chunk[j] != b'm' {
                j += 1;
            }
            if j < chunk.len() {
                let is_release = chunk[j] == b'm';
                if let Some(ev) = parse_one_mouse(&chunk[body_start..j], is_release) {
                    events.push(ev);
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    events
}

fn parse_one_mouse(body: &[u8], is_release: bool) -> Option<MouseEvent> {
    let s = std::str::from_utf8(body).ok()?;
    let mut it = s.split(';');
    let b: u32 = it.next()?.parse().ok()?;
    let x: u16 = it.next()?.parse().ok()?;
    let y: u16 = it.next()?.parse().ok()?;
    let kind = if b & 0x40 != 0 {
        // Wheel: low bit selects direction.
        if b & 1 == 0 {
            MouseKind::WheelUp
        } else {
            MouseKind::WheelDown
        }
    } else if is_release {
        MouseKind::Release
    } else if b & 0x20 != 0 {
        MouseKind::Drag
    } else if b & 0x03 == 0 {
        MouseKind::Press
    } else {
        MouseKind::Other
    };
    Some(MouseEvent {
        x: x.saturating_sub(1),
        y: y.saturating_sub(1),
        kind,
    })
}

// ---- OSC 52 clipboard ----

/// Build an OSC 52 sequence that copies `text` to the system clipboard.
pub fn osc52_copy(text: &str) -> Vec<u8> {
    let mut out = b"\x1b]52;c;".to_vec();
    out.extend_from_slice(base64_encode(text.as_bytes()).as_bytes());
    out.push(0x07); // BEL terminator
    out
}

/// Minimal standard base64 (no padding-free tricks), dependency-free.
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn osc52_wraps_base64() {
        let seq = osc52_copy("hi");
        assert_eq!(seq, b"\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn parse_wheel_and_buttons() {
        // Wheel up at col 10 row 5 (1-based in the wire form).
        let up = parse_mouse(b"\x1b[<64;10;5M");
        assert_eq!(up.len(), 1);
        assert_eq!(up[0].kind, MouseKind::WheelUp);
        assert_eq!((up[0].x, up[0].y), (9, 4));

        let down = parse_mouse(b"\x1b[<65;1;1M");
        assert_eq!(down[0].kind, MouseKind::WheelDown);

        let press = parse_mouse(b"\x1b[<0;3;7M");
        assert_eq!(press[0].kind, MouseKind::Press);
        let drag = parse_mouse(b"\x1b[<32;4;7M");
        assert_eq!(drag[0].kind, MouseKind::Drag);
        let release = parse_mouse(b"\x1b[<0;5;7m");
        assert_eq!(release[0].kind, MouseKind::Release);
    }

    #[test]
    fn selection_row_major_contains() {
        let s = Selection {
            start: (2, 0),
            end: (3, 1),
        };
        assert!(!s.contains(1, 0));
        assert!(s.contains(2, 0));
        assert!(s.contains(9, 0)); // rest of the start row
        assert!(s.contains(0, 1)); // start of the end row
        assert!(s.contains(3, 1));
        assert!(!s.contains(4, 1));
    }
}
