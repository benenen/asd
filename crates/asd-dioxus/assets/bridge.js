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

  var term, fit;
  try {
    term = new window.GhosttyWeb.Terminal({
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

    // Track the pane: debounce bursts of layout changes into one fit.
    var fitTimer = null;
    new ResizeObserver(function() {
      if (fitTimer) clearTimeout(fitTimer);
      fitTimer = setTimeout(doFit, 50);
    }).observe(el);

    // Rust → JS entry points (called via evaluate_script).
    window.__asdTerm = term;
    window.__asdWrite = function(data) {
      if (window.__asdTerm) { window.__asdTerm.write(data); }
    };
    window.__asdReset = function() {
      if (!window.__asdTerm) return;
      if (typeof window.__asdTerm.reset === 'function') {
        window.__asdTerm.reset();
      } else {
        // RIS: full reset for terminals without an xterm-style reset().
        window.__asdTerm.write('\x1bc');
      }
    };

    el.addEventListener('mousedown', function() {
      if (typeof term.focus === 'function') term.focus();
    });
    if (typeof term.focus === 'function') term.focus();

    log('terminal ' + term.cols + 'x' + term.rows);
  } catch (e) {
    log('terminal FAILED: ' + (e && e.message ? e.message : String(e)));
    return;
  }

  log('bridge ready');
  await keepalive();
  log('bridge ended');
})();
