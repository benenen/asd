//! `asd-dioxus`: GPU terminal client built with Dioxus Desktop + ghostty-web.
//!
//! Library crate combined into the single `asd` binary by the root package's
//! `dioxus` feature (the iced client `asd-gui` sits behind `iced`). Same
//! boundary contract as `asd-gui`: no PTY/process management — remote hosts go
//! through pure-Rust SSH ([`russh`]), the local daemon through its socket.

#![allow(non_snake_case)]

mod app;
mod bridge;
mod conn;
mod model;
mod settings;
mod ssh;

use std::sync::OnceLock;

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
use dioxus::prelude::*;

/// The npm vendor bundle (ghostty-web with its WASM base64-inlined, plus any
/// future webview deps), built and minified by npm/esbuild via `build.rs`.
/// Injected with `document::Script` in [`app::App`]. Inlined rather than an
/// `asset!()` file so the shipped `asd` stays one self-contained binary.
pub(crate) const VENDOR_JS: &str = include_str!(concat!(env!("OUT_DIR"), "/vendor.js"));

/// App stylesheet (palette mirrors asd-gui's theme.rs), injected with
/// `document::Style` in [`app::App`].
pub(crate) const APP_CSS: &str = include_str!("../assets/app.css");

/// The session named on the command line, auto-selected once the local list
/// arrives. Dioxus's launch API takes a plain component, so this rides in a
/// static rather than a prop.
static PREFERRED: OnceLock<Option<String>> = OnceLock::new();

pub(crate) fn preferred_session() -> Option<String> {
    PREFERRED.get().cloned().flatten()
}

/// Open the client window; `session` preselects a local session by name.
/// Returns when the window closes.
pub fn run(session: Option<String>) -> anyhow::Result<()> {
    let _ = PREFERRED.set(session);
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_thread_names(true)
        .with_writer(std::io::stderr)
        .try_init();

    let cfg = Config::new().with_window(
        WindowBuilder::new()
            .with_title("asd")
            .with_inner_size(LogicalSize::new(1100.0, 680.0)),
    );

    LaunchBuilder::desktop().with_cfg(cfg).launch(app::App);
    Ok(())
}
