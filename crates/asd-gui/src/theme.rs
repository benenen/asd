//! Design tokens for the two-pane shell (sidebar + terminal + status bar).
//!
//! The palette is a warm-slate ground with a **duo accent that encodes host
//! origin** — amber for local sessions, cyan for SSH remotes — so "which
//! machine is this on" reads from color before any label. Semantic colors
//! (`OK`/`ALERT`) are kept separate from the accent and used sparingly.

use iced::Color;

/// Build a color from 8-bit sRGB components at compile time.
const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
}

// Grounds.
pub const VOID: Color = rgb(0x0E, 0x11, 0x16); // app frame
pub const PANEL: Color = rgb(0x14, 0x18, 0x1F); // sidebar
pub const SCREEN: Color = rgb(0x0B, 0x0D, 0x11); // terminal surface
pub const HEAD: Color = rgb(0x0E, 0x12, 0x18); // pane headers
pub const STATUS: Color = rgb(0x12, 0x16, 0x1C); // status bar
pub const HOVER: Color = rgb(0x17, 0x1D, 0x25); // row hover / press

// Lines.
pub const LINE: Color = rgb(0x23, 0x2A, 0x34);
pub const LINE_SOFT: Color = rgb(0x1B, 0x21, 0x2A);

// Text.
pub const TEXT: Color = rgb(0xE7, 0xE2, 0xD6); // warm off-white
pub const BRIGHT: Color = rgb(0xFF, 0xFF, 0xFF);
pub const MUTED: Color = rgb(0x8B, 0x94, 0xA2);
pub const DIM: Color = rgb(0x5A, 0x64, 0x72);

// Duo accent (host origin).
pub const LOCAL: Color = rgb(0xF3, 0xB2, 0x4C); // amber
pub const REMOTE: Color = rgb(0x54, 0xC7, 0xDA); // cyan

// Semantic (not the accent).
pub const OK: Color = rgb(0x79, 0xD1, 0x8C);
pub const ALERT: Color = rgb(0xE5, 0x59, 0x5E);

/// Accent for a host by origin: amber (local) or cyan (remote).
pub fn rail(is_remote: bool) -> Color {
    if is_remote { REMOTE } else { LOCAL }
}

/// A translucent tint of `c` for selection backgrounds.
pub fn tint(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

/// The keycap fill used by the `a s d` wordmark.
pub const KEYCAP: Color = rgb(0x1B, 0x22, 0x2C);
pub const KEYCAP_EDGE: Color = rgb(0x2B, 0x35, 0x42);
