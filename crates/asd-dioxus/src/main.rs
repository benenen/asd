//! `asd-dioxus`: GPU terminal client built with Dioxus Desktop + ghostty-web.

#![allow(non_snake_case)]

mod app;
mod bridge;
mod conn;
mod model;

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
use dioxus::prelude::*;

/// ghostty-web 0.4.0 ESM bundle (WASM already base64-inlined), stripped of
/// `export {}` and wrapped in an IIFE that exposes `window.GhosttyWeb`.
const GHOSTTY_WEB_JS: &str = include_str!("ghostty-web-bundle.js");

const CUSTOM_HEAD: &str = r#"
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  html, body { height: 100%; overflow: hidden; }
  body { font-family: 'JetBrains Mono','SF Mono',Menlo,monospace; background:#0B0D11; color:#E7E2D6; }
  .app-container { display:flex; flex-direction:row; height:100vh; }
  .sidebar { width:260px; min-width:260px; background:#14181F; display:flex; flex-direction:column; border-right:1px solid #232A34; }
  .brand { padding:16px; border-bottom:1px solid #232A34; }
  .brand .logo { font-size:18px; font-weight:bold; display:block; }
  .brand .tagline { font-size:10px; color:#5A6472; display:block; }
  .section-label { padding:12px 16px 6px; font-size:10px; color:#5A6472; letter-spacing:1px; }
  .host-list { flex:1; overflow-y:auto; padding:4px 8px; }
  .host-group { padding:2px 0; }
  .host-head { padding:8px 12px; font-size:10px; color:#8B94A2; display:flex; align-items:center; gap:8px; }
  .host-dot { width:7px; height:7px; border-radius:50%; background:#79D18C; }
  .session-row { padding:6px 12px; margin:2px 0; border-radius:6px; cursor:pointer; display:flex; align-items:center; gap:8px; font-size:13px; }
  .session-row:hover { background:#171D25; }
  .session-row.active { background:rgba(243,178,76,0.12); border-left:2px solid #F3B24C; }
  .session-dot { width:8px; height:8px; border-radius:50%; border:2px solid #F3B24C; }
  .session-dot.attached { background:#F3B24C; }
  .session-name { flex:1; }
  .session-cmd { font-size:11px; color:#8B94A2; margin-left:auto; }
  .remote-input { width:100%; padding:8px 12px; margin:8px 0; border-radius:8px; border:1px solid #2C3542; background:#0B0D11; color:#E7E2D6; font-family:inherit; font-size:12px; outline:none; }
  .remote-input::placeholder { color:#5A6472; }
  .sidebar-footer { padding:12px 16px; border-top:1px solid #232A34; font-size:10px; color:#5A6472; display:flex; justify-content:space-between; }
  .terminal-pane { flex:1; display:flex; flex-direction:column; }
  .term-head { height:40px; display:flex; align-items:center; padding:0 14px; background:#0E1218; border-bottom:1px solid #1B212A; font-size:12px; gap:10px; }
  .term-body { flex:1; overflow:hidden; position:relative; }
  .term-inner { width:100%; height:100%; position:absolute; }
  .term-inner canvas { position:absolute; top:0; left:0; }
  .status-bar { height:28px; display:flex; align-items:center; padding:0 14px; background:#12161C; border-top:1px solid #232A34; font-size:11px; color:#5A6472; justify-content:space-between; }
  .status-dot { width:7px; height:7px; border-radius:50%; display:inline-block; margin-right:6px; }
  .settings-btn { background:none; border:1px solid #232A34; border-radius:4px; color:#8B94A2; padding:2px 8px; cursor:pointer; font-family:inherit; }
  .settings-btn:hover { background:#171D25; }
</style>
"#;

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .try_init();

    // Prepend the ghostty-web bundle <script> before CSS.
    let custom_head = format!(
        "<script>{}</script>{}",
        GHOSTTY_WEB_JS, CUSTOM_HEAD
    );

    let cfg = Config::new()
        .with_window(
            WindowBuilder::new()
                .with_title("asd")
                .with_inner_size(LogicalSize::new(1100.0, 680.0)),
        )
        .with_custom_head(custom_head);

    LaunchBuilder::desktop()
        .with_cfg(cfg)
        .launch(app::App);
}
