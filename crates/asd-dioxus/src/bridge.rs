//! Rust ↔ JavaScript bridge for ghostty-web communication.
//!
//! JS → Rust: `dioxus.send()` via Dioxus eval bridge (for keystrokes/resize).
//! Rust → JS: `window.__asdWrite()` via direct evaluate_script (for PTY output).
//!   This bypasses the Dioxus eval bridge which appears broken for Rust→JS in 0.8-alpha.

use serde::{Deserialize, Serialize};

/// Messages from JS (ghostty-web callbacks) — sent via dioxus.send().
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum JsMessage {
    #[serde(rename = "input")] Input { data: String },
    #[serde(rename = "resize")] Resize { cols: u16, rows: u16 },
    #[serde(rename = "status")] Status { msg: String },
}

/// Messages to JS (terminal output) — NOT used via eval bridge.
/// Retained for reference; actual Rust→JS now uses window.__asdWrite().
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ToJs {
    Write(String),
    Resize { cols: u16, rows: u16 },
}

/// Self-contained JS bridge.
/// Rust→JS output now comes via `window.__asdWrite(data)` (called by direct
/// evaluate_script), not through dioxus.recv().
pub const BRIDGE_JS: &str = r#"
(async function() {
  var log = function(msg) {
    console.log('[asd]', msg);
    dioxus.send({ type: 'status', msg: msg });
  };

  log('bridge starting');

  if (typeof window.GhosttyWeb === 'undefined') {
    log('FATAL: window.GhosttyWeb undefined');
    return;
  }

  try {
    await window.GhosttyWeb.init();
    log('ghostty-web initialized');
  } catch (e) {
    log('init FAILED: ' + (e && e.message ? e.message : String(e)));
    return;
  }

  var el = null;
  for (var i = 0; i < 50; i++) {
    el = document.getElementById('terminal');
    if (el && el.offsetWidth > 0 && el.offsetHeight > 0) break;
    await new Promise(function(r) { setTimeout(r, 100); });
  }
  if (!el) { log('FATAL: #terminal not found'); return; }

  var term;
  try {
    term = new window.GhosttyWeb.Terminal({
      fontSize: 14,
      fontFamily: 'Menlo, Monaco, monospace',
      theme: {
        background: '#0B0D11', foreground: '#E7E2D6', cursor: '#E7E2D6',
        selectionBackground: '#4A5568',
        black: '#1A202C', red: '#E5595E', green: '#79D18C', yellow: '#F3B24C',
        blue: '#54C7DA', magenta: '#B98EFF', cyan: '#54C7DA', white: '#E7E2D6',
        brightBlack: '#4A5568', brightRed: '#FF6B6B', brightGreen: '#9AE6B4',
        brightYellow: '#FBD38D', brightBlue: '#76E4F7', brightMagenta: '#D6BCFF',
        brightCyan: '#76E4F7', brightWhite: '#FFFFFF',
      },
      allowTransparency: false, cursorBlink: true, cursorStyle: 'bar',
    });
    term.open(el);
    log('terminal: ' + term.cols + 'x' + term.rows);

    var canvas = el.querySelector('canvas');
    if (canvas) {
      canvas.style.width = '100%';
      canvas.style.height = '100%';
      log('canvas ' + canvas.width + 'x' + canvas.height);
    } else {
      log('NO CANVAS');
    }

    // Expose the terminal globally for direct evaluate_script writes.
    window.__asdTerm = term;
    // Global write function called by Rust via evaluate_script.
    window.__asdWrite = function(data) {
      if (window.__asdTerm) {
        window.__asdTerm.write(data);
      }
    };

    term.write('\r\n\x1b[1;32m=== asd ready ===\x1b[m\r\n$ ');
    log('test write done');
  } catch (e) {
    log('terminal FAILED: ' + (e && e.message ? e.message : String(e)));
    return;
  }

  // JS → Rust: keystrokes and resize events.
  term.onData(function(data) {
    dioxus.send({ type: 'input', data: data });
  });
  term.onResize(function(info) {
    dioxus.send({ type: 'resize', cols: info.cols, rows: info.rows });
  });

  // Keep the eval channel alive by awaiting on it (so the coroutine stays open).
  // Rust→JS data now comes via window.__asdWrite, not through this channel.
  log('bridge ready, waiting for events');
  while (true) {
    try {
      var msg = await dioxus.recv();
      // dioxus.recv is only used for keepalive now.
      // Actual data comes via window.__asdWrite.
      if (msg === null || msg === undefined) continue;
      log('unexpected recv: ' + JSON.stringify(msg).substring(0, 60));
    } catch (e) {
      log('recv error: ' + (e && e.message ? e.message : String(e)));
      break;
    }
  }
  log('bridge ended');
})();
"#;
