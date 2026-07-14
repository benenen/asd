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
    fn render_snapshot(&mut self) -> RenderSnapshot;

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

    /// Selection text (delivered in M1; default returns an empty string).
    fn selection_text(&self, sel: Selection) -> String {
        let _ = sel;
        String::new()
    }
}
