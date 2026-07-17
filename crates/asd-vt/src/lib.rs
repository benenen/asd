//! asd's VT backend boundary (spec §3/§6).
//!
//! This layer is the **escape-hatch boundary** for libghostty-vt: the daemon
//! and GUI program only against the [`VtBackend`] trait and the
//! [`RenderSnapshot`] plain data; libghostty-vt API churn (0.2.x is
//! unstable) is absorbed entirely within this crate, and the alternative
//! backend (alacritty_terminal) would also be swapped in behind this
//! boundary.
//!
//! Dependency contract: iced/wgpu, portable-pty, and asd-proto are forbidden.

pub mod clip;
mod ghostty;
mod types;

pub use ghostty::GhosttyVt;
pub use types::{
    CellSnapshot, CellWidth, CursorShape, CursorSnapshot, Key, KeyEvent, Mods, RenderSnapshot, Rgb,
    Selection, StyleFlags, UnderlineKind,
};

/// VT backend trait (spec §6).
///
/// Implementations are expected to be `!Send`, owned exclusively by their
/// holding thread; only plain data such as [`RenderSnapshot`] and `Vec<u8>`
/// crosses threads.
///
/// Tweaks relative to the spec draft signatures (the spec allows
/// "signatures may be tweaked during implementation"):
/// - `snapshot_vt`/`render_snapshot`/`encode_key` take `&mut self` — the
///   underlying formatter/render state/key encoder are all mutable native
///   state;
/// - adds [`VtBackend::take_pty_responses`]: reply bytes the terminal
///   produces for DA/DSR-style queries during `feed`; the host (the daemon
///   session thread) must take them out and write them back to the pty,
///   otherwise capability probes in programs like vim hang waiting.
pub trait VtBackend: Sized {
    /// Create a `cols × rows` terminal keeping `scrollback` lines of history.
    fn new(cols: u16, rows: u16, scrollback: usize) -> Self;

    /// Feed in pty output.
    fn feed(&mut self, bytes: &[u8]);

    /// Resize the terminal (primary screen content reflows as needed).
    fn resize(&mut self, cols: u16, rows: u16);

    /// Current screen → VT sequence (attach snapshot; includes cursor/style/
    /// modes/palette).
    ///
    /// Contract (spec §8): after feeding the return value to a fresh terminal
    /// of the same size, both `render_snapshot`s must match (snapshot
    /// fidelity).
    fn snapshot_vt(&mut self) -> Vec<u8>;

    /// Produce a plain-data render snapshot to hand to the GUI across threads.
    /// Renders whatever the viewport currently shows — call [`set_scroll`]
    /// first to render a scrolled-back view.
    ///
    /// [`set_scroll`]: VtBackend::set_scroll
    fn render_snapshot(&mut self) -> RenderSnapshot;

    /// Position the render viewport `lines_up` lines above the live bottom
    /// (0 = follow the live screen). Clamped to [`scrollback_rows`]. Affects
    /// the next [`render_snapshot`]/[`selection_text`].
    ///
    /// [`scrollback_rows`]: VtBackend::scrollback_rows
    /// [`render_snapshot`]: VtBackend::render_snapshot
    /// [`selection_text`]: VtBackend::selection_text
    fn set_scroll(&mut self, lines_up: usize) {
        let _ = lines_up;
    }

    /// Number of scrollback rows above the live screen (max scroll-up).
    fn scrollback_rows(&mut self) -> usize {
        0
    }

    /// Whether the program is on the alternate screen (vim/less/htop/...).
    fn is_alt_screen(&mut self) -> bool {
        false
    }

    /// Whether the program has any mouse tracking mode active.
    fn is_mouse_tracking(&mut self) -> bool {
        false
    }

    /// Whether the program is currently inside a synchronized-output update
    /// (DEC private mode 2026, `?2026h` … `?2026l`) — i.e. mid-way through an
    /// atomic screen repaint. A renderer should keep showing the previous
    /// complete frame until this clears, so it never paints a half-drawn one.
    /// Default returns false (never synchronized).
    fn synchronized_output(&mut self) -> bool {
        false
    }

    /// The terminal title as set by the program (OSC 0/2); empty when never
    /// set. The default implementation returns empty.
    fn title(&mut self) -> String {
        String::new()
    }

    /// The DEC private mode numbers the program currently has enabled among
    /// the mouse set — tracking level (9/1000/1002/1003) and encoding
    /// (1005/1006/1015/1016), ascending. Empty = no mouse (native selection is
    /// fine). A client mirrors these onto a host terminal so mouse events reach
    /// the program in the exact encoding it expects. Default returns empty.
    fn mouse_modes(&mut self) -> Vec<u16> {
        Vec::new()
    }

    /// Encode a keystroke into input bytes according to the current terminal
    /// modes (DECCKM, kitty protocol, etc.).
    fn encode_key(&mut self, ev: KeyEvent) -> Vec<u8>;

    /// Take the query reply bytes accumulated during `feed` (DA, DSR, DECRQM,
    /// etc.); the caller is responsible for writing them back to the pty.
    /// The default implementation returns empty.
    fn take_pty_responses(&mut self) -> Vec<u8> {
        Vec::new()
    }

    /// Total number of rows in "screen space": scrollback history plus the
    /// live screen. Row 0 is the oldest scrollback line; the live view is the
    /// bottom `rows` of this space. Backs the M1 scrollback viewer.
    fn history_len(&mut self) -> usize {
        0
    }

    /// Read the screen-space row window `[start, start + count)` as plain
    /// UTF-8 text lines (trailing blanks trimmed), clamped to the available
    /// range. See [`VtBackend::history_len`] for the coordinate space.
    fn fetch_history(&mut self, start: u32, count: u32) -> Vec<Vec<u8>> {
        let _ = (start, count);
        Vec::new()
    }

    /// Text of a selection over the current viewport (viewport coordinates;
    /// call [`set_scroll`] first so coordinates match what is displayed).
    /// Uses copy semantics: soft-wrapped lines unwrapped, trailing blanks
    /// trimmed. Default returns an empty string.
    ///
    /// [`set_scroll`]: VtBackend::set_scroll
    fn selection_text(&mut self, sel: Selection) -> String {
        let _ = sel;
        String::new()
    }

    /// Text of a selection given in screen-space coordinates (`(x, row)` with
    /// row 0 = the oldest scrollback line; see [`history_len`]). Unlike
    /// [`selection_text`] this is independent of the current viewport/scroll,
    /// so it captures a selection that spans rows scrolled off-screen — the
    /// natural fit for a content-anchored selection. Same copy semantics
    /// (unwrap soft-wraps, trim trailing blanks). Default returns empty.
    ///
    /// [`history_len`]: VtBackend::history_len
    /// [`selection_text`]: VtBackend::selection_text
    fn selection_text_screen(&mut self, start: (u16, u32), end: (u16, u32)) -> String {
        let _ = (start, end);
        String::new()
    }
}
