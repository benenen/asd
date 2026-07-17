//! Rust ↔ JavaScript bridge for ghostty-web communication.
//!
//! JS → Rust: `dioxus.send()` via Dioxus eval bridge (keystrokes/resize/status).
//! Rust → JS: `window.__asdWrite()` / `window.__asdReset()` via direct
//!   evaluate_script (PTY output and the pre-snapshot terminal reset). This
//!   bypasses the Dioxus eval bridge which is broken for Rust→JS in 0.8-alpha.

use serde::Deserialize;

/// Messages from JS (ghostty-web callbacks) — sent via dioxus.send().
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum JsMessage {
    #[serde(rename = "input")]
    Input { data: String },
    #[serde(rename = "resize")]
    Resize { cols: u16, rows: u16 },
    #[serde(rename = "status")]
    Status { msg: String },
}

/// Self-contained JS bridge (see `assets/bridge.js`): initializes ghostty-web,
/// opens the terminal in `#terminal`, keeps it fitted to the pane, and
/// forwards input/resize to Rust.
///
/// Rust→JS entry points installed on `window`:
///   * `__asdWrite(data)` — write PTY bytes (string) to the terminal;
///   * `__asdReset()` — full terminal reset, called right before a new
///     session's Snapshot so no old content survives a switch.
pub const BRIDGE_JS: &str = include_str!("../assets/bridge.js");
