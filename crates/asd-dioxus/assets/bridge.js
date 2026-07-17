// asd ↔ ghostty-web bridge, injected via document::eval (see src/bridge.rs).
//
// JS → Rust: window.__asdSend → dioxus.send({type: 'input'|'resize'|'status'}).
//   The first eval's channel can go deaf when it races the page load
//   (0.8-alpha), so Rust re-issues the eval until it hears something; every
//   run rebinds window.__asdSend to its own (fresh) channel, and all sends —
//   including callbacks registered by an earlier run — go through that
//   binding, so the newest live channel always carries the traffic.
// Rust → JS: window.__asdWrite(data) / window.__asdReset(), called through
//   wry's evaluate_script (the Dioxus eval bridge is broken Rust→JS too).
//
// Self-healing: the terminal is built by a function so it can be recreated
// if the WASM side ever dies (e.g. after an output storm) — repeated
// __asdWrite failures dispose and rebuild it, and Rust re-attaches the
// active session when it sees the 'terminal recreated' status marker.
(async function() {
  window.__asdSend = function(m) { try { dioxus.send(m); } catch (e) {} };
  var send = function(m) { window.__asdSend(m); };
  var log = function(msg) {
    console.log('[asd]', msg);
    send({ type: 'status', msg: msg });
  };

  // Keep this eval's channel open; Rust ignores whatever recv returns.
  var keepalive = async function() {
    while (true) {
      try {
        var m = await dioxus.recv();
        if (m === null || m === undefined) continue;
      } catch (e) {
        log('recv error: ' + (e && e.message ? e.message : String(e)));
        break;
      }
    }
  };

  // A re-issued eval adopts messaging and re-announces the current state; the
  // original run keeps driving the terminal.
  if (window.__asdBridgeStarted) {
    log('bridge alive');
    if (window.__asdTerm) {
      send({ type: 'resize', cols: window.__asdTerm.cols, rows: window.__asdTerm.rows });
      log('bridge ready');
    }
    await keepalive();
    return;
  }
  window.__asdBridgeStarted = true;

  window.addEventListener('error', function(e) {
    log('js error: ' + e.message);
  });
  window.addEventListener('unhandledrejection', function(e) {
    log('unhandled rejection: ' + String(e.reason));
  });

  log('bridge starting');

  // The eval can outrun the custom-head <script> that defines GhosttyWeb;
  // wait for it rather than bailing (and let a later eval retry if it truly
  // never shows up).
  var waited = 0;
  while (typeof window.GhosttyWeb === 'undefined' && waited < 15000) {
    await new Promise(function(r) { setTimeout(r, 100); });
    waited += 100;
  }
  if (typeof window.GhosttyWeb === 'undefined') {
    log('FATAL: window.GhosttyWeb undefined');
    window.__asdBridgeStarted = false;
    return;
  }

  try {
    var slow = setTimeout(function() { log('init still running after 5s'); }, 5000);
    await window.GhosttyWeb.init();
    clearTimeout(slow);
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

  // Build (or rebuild) the terminal inside #terminal.
  var buildTerm = function() {
    var term = new window.GhosttyWeb.Terminal({
      fontSize: 14,
      fontFamily: "'JetBrains Mono', Menlo, Monaco, monospace",
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
    var fit = null;
    if (window.GhosttyWeb.FitAddon) {
      try {
        fit = new window.GhosttyWeb.FitAddon();
        term.loadAddon(fit);
      } catch (e) {
        fit = null;
        log('fit addon unavailable: ' + String(e));
      }
    }
    term.open(el);

    // JS → Rust: keystrokes and size. Registered before the first fit() so
    // Rust learns the real grid before the first attach.
    term.onData(function(data) {
      send({ type: 'input', data: data });
    });
    term.onResize(function(info) {
      send({ type: 'resize', cols: info.cols, rows: info.rows });
    });

    var doFit = function() {
      if (fit) {
        try { fit.fit(); } catch (e) { /* pane momentarily 0-sized */ }
      }
    };
    doFit();
    // Report the initial grid even if fit() did not change it (no onResize).
    send({ type: 'resize', cols: term.cols, rows: term.rows });
    window.__asdDoFit = doFit;
    return term;
  };

  var recreateTerm = function() {
    log('recreating terminal');
    try {
      if (window.__asdTerm && typeof window.__asdTerm.dispose === 'function') {
        window.__asdTerm.dispose();
      }
    } catch (e) { /* already broken — that's why we're here */ }
    el.innerHTML = '';
    try {
      window.__asdTerm = buildTerm();
      window.__asdWriteErrors = 0;
      // Rust re-attaches the active session on this marker (fresh Snapshot
      // into the fresh terminal).
      log('terminal recreated');
    } catch (e) {
      log('terminal recreate FAILED: ' + (e && e.message ? e.message : String(e)));
    }
  };

  try {
    window.__asdTerm = buildTerm();
    window.__asdWriteErrors = 0;

    // Track the pane: debounce bursts of layout changes into one fit.
    var fitTimer = null;
    new ResizeObserver(function() {
      if (fitTimer) clearTimeout(fitTimer);
      fitTimer = setTimeout(function() {
        if (window.__asdDoFit) window.__asdDoFit();
      }, 50);
    }).observe(el);

    // Rust → JS entry points (called via evaluate_script).
    window.__asdWrite = function(data) {
      if (!window.__asdTerm) return;
      try {
        window.__asdTerm.write(data);
        window.__asdWriteErrors = 0;
      } catch (e) {
        window.__asdWriteErrors = (window.__asdWriteErrors || 0) + 1;
        log('write error #' + window.__asdWriteErrors + ': '
            + (e && e.message ? e.message : String(e)));
        // A wedged WASM terminal never recovers on its own; rebuild it.
        if (window.__asdWriteErrors >= 3) recreateTerm();
      }
    };
    window.__asdReset = function() {
      if (!window.__asdTerm) return;
      try {
        if (typeof window.__asdTerm.reset === 'function') {
          window.__asdTerm.reset();
        } else {
          // RIS: full reset for terminals without an xterm-style reset().
          window.__asdTerm.write('\x1bc');
        }
      } catch (e) {
        log('reset error: ' + (e && e.message ? e.message : String(e)));
        recreateTerm();
      }
    };

    el.addEventListener('mousedown', function() {
      var t = window.__asdTerm;
      if (t && typeof t.focus === 'function') t.focus();
    });
    if (typeof window.__asdTerm.focus === 'function') window.__asdTerm.focus();

    log('terminal ' + window.__asdTerm.cols + 'x' + window.__asdTerm.rows);
  } catch (e) {
    log('terminal FAILED: ' + (e && e.message ? e.message : String(e)));
    return;
  }

  log('bridge ready');
  await keepalive();
  log('bridge ended');
})();
