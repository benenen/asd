# asd-dioxus

GPU terminal client built with [Dioxus Desktop][dioxus] + [ghostty-web][ghostty-web], replacing the earlier `asd-gui` (iced/wgpu).

The webview hosts ghostty-web — Ghostty's WASM terminal emulator with an xterm.js-compatible API — while Rust handles daemon communication, session management, and SSH via russh.

## Architecture

```
┌─ Rust (Dioxus Desktop) ─────────────────────────┐
│                                                  │
│  app.rs          ┌─ conn.rs ──┐                  │
│  (sidebar +      │ UDS / SSH  │    asd-daemon    │
│   terminal pane) │ protocol   │◄═════════════════│
│       │          └────────────┘   Unix socket    │
│       │                                          │
│  use_coroutine                                   │
│       │                                          │
│       ├── document::eval(BRIDGE_JS) ──────┐      │
│       │   (JS→Rust: keystrokes/resize)    │      │
│       │                                   ▼      │
│       └── desktop.webview.evaluate_script()      │
│           (Rust→JS: PTY output)          ┌───────┤
│                                          │       │
└──────────────────────────────────────────┼───────┘
                                           ▼
┌─ JS (wry WebView) ───────────────────────────────┐
│                                                   │
│  window.GhosttyWeb  ← bundled IIFE (682 KB)       │
│       │                                           │
│       ├── init()  → loads WASM (base64 inlined)  │
│       └── Terminal → renders to <canvas>          │
│                                                   │
│  window.__asdWrite(data)  ← Rust→JS PTY output   │
│  dioxus.send({...})       → JS→Rust events        │
│                                                   │
└───────────────────────────────────────────────────┘
```

### Bridge direction

| Direction | Mechanism | Notes |
|---|---|---|
| JS → Rust | `dioxus.send()` via Dioxus eval bridge | Keystrokes (`onData`), resize (`onResize`), status messages |
| Rust → JS | `DesktopContext::webview.evaluate_script()` → `window.__asdWrite()` | PTY output (Snapshot / Output frames). Bypasses Dioxus eval bridge because `eval.send()` is broken in 0.8-alpha (returns `Ok` but JS never receives) |

### Why direct evaluate_script for Rust→JS

Dioxus 0.8-alpha's `eval.send()` calls `Query::send()` which generates `window.getQuery(N).rustSend(data)` and executes it via wry's `evaluate_script`. The call returns `Ok`, but the data never arrives in the JS `dioxus.recv()` loop. Root cause is unclear (possible channel mismatch or GC timing in the native eval bridge). The workaround: call `window.__asdWrite(JSON.parse(json))` directly through the same `evaluate_script` API, which works reliably. JSON serialization via `serde_json` handles all control-character escaping.

### Offline ghostty-web bundle

The `ghostty-web` npm package (0.4.0, 682 KB) is processed and embedded at compile time:

1. The ESM bundle at `node_modules/ghostty-web/dist/ghostty-web.js` has WASM already base64-inlined — no separate `.wasm` file needed.
2. The `export { ... }` statement is stripped and the code is wrapped in an IIFE that exposes `window.GhosttyWeb = { Terminal, init, Ghostty, FitAddon }`.
3. The resulting `src/ghostty-web-bundle.js` is embedded via `include_str!()` in `main.rs` and injected as the first `<script>` tag in `CUSTOM_HEAD`.

To update ghostty-web: `npm install ghostty-web@latest` then re-run the bundle processing (see `src/ghostty-web-bundle.js` generation in the build notes).

## Files

| File | Purpose |
|---|---|
| `src/main.rs` | Entry point: Dioxus Desktop config, custom head (CSS + ghostty-web bundle) |
| `src/app.rs` | App component: sidebar (hosts/sessions), terminal pane, coroutine wiring |
| `src/bridge.rs` | `BRIDGE_JS` — self-contained JS bridge that creates the terminal and runs the receive loop |
| `src/conn.rs` | Daemon connection: UDS handshake, session polling, attach/detach, PTY I/O |
| `src/model.rs` | `Session` struct (from `asd_proto::SessionInfo`) |
| `src/ghostty-web-bundle.js` | Processed ghostty-web IIFE (682 KB, embedded at compile time) |

## Build & run

```bash
# Build
cargo build -p asd-dioxus

# Run (requires a running asd daemon)
cargo run -p asd-dioxus

# With debug logging
RUST_LOG=debug cargo run -p asd-dioxus
```

### Prerequisites

- **asd daemon** must be running (`asd daemon` or `cargo run -- daemon` from workspace root).
- **macOS**: wry uses WKWebView — works out of the box.
- **Windows/Linux**: wry uses WebView2 / WebKitGTK respectively.

### Debugging the webview

Right-click the terminal area and select "Inspect" to open WebKit developer tools (enabled by default in debug builds). JS console logs appear there, not in the Rust terminal output.

## Key design decisions

**Single connection, multi-attach**: The `conn::run` task holds one UDS connection to the daemon. Switching sessions sends `Detach` + `Attach` on the same connection. The `attaching` flag drops stale `Output` frames during the switch.

**Eager channel creation**: The `daemon_cmd_tx`/`daemon_cmd_rx` channel pair is created before the coroutine, so sidebar click handlers always have a valid sender — no `None`-then-`Some` race condition.

**Deferred auto-attach**: Session auto-attach waits for the JS bridge to signal readiness (`"bridge ready"` status message). This prevents `evaluate_script` calls from racing with `window.__asdWrite` setup.

**Terminal test banner**: The bridge writes `=== asd ready ===` in green immediately after creation. If this renders but shell content doesn't, the issue is in the data path (daemon/Rust→JS). If nothing renders at all, the issue is in ghostty-web (WASM init, canvas, font).

[dioxus]: https://dioxuslabs.com
[ghostty-web]: https://www.npmjs.com/package/ghostty-web
