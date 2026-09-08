# asd-dioxus

Terminal client built with [Dioxus Desktop][dioxus] + [ghostty-web][ghostty-web]: a host-grouped session sidebar, saved SSH connections, a settings overlay, and pure-Rust SSH remotes.

This is a **library crate**: the root `asd` binary combines it via the `dioxus` feature. Entry point: `asd_dioxus::run(session: Option<String>)`.

## Architecture

```
┌─ Rust ────────────────────────────────────────────────────────┐
│  app.rs                                                        │
│   ├─ App (rsx): sidebar + terminal pane + status bar +         │
│   │             settings overlay                               │
│   └─ supervisor (use_coroutine):                               │
│        routes AppCmd → per-host actors, folds UiEvent → signals│
│                                                                │
│  conn.rs — one actor per host on a dedicated tokio runtime     │
│   ├─ local: platform stream → asd-daemon                       │
│   └─ remote: ssh.rs (russh) → `asd attach --stdio` far end     │
│                                                                │
│  model.rs / settings.rs — Send data: hosts, sessions,          │
│   saved SSH connections (<platform data dir>/config.json)      │
└────────────┬───────────────────────────────────▲───────────────┘
             │ evaluate_script:                  │ dioxus.send:
             │  __asdWrite / __asdReset          │  input/resize/status
┌────────────▼───────────────────────────────────┴───────────────┐
│  JS (wry WebView)                                              │
│   window.GhosttyWeb ← vendor bundle (npm + esbuild, minified)  │
│   assets/bridge.js  ← terminal setup, FitAddon, callbacks      │
└────────────────────────────────────────────────────────────────┘
```

Session semantics: each host has a control connection and a dedicated event connection, both using the same local or SSH transport. The event feed supplies the sidebar without list polling and reconnects using its last accepted cursor; invalid ordering requests an authoritative reset. Only the viewed session is attached; switching sends `Detach`+`Attach` on the control connection. Attach convergence drops superseded Snapshots/Output, and every `UiEvent::Bytes` carries the exact session identity as well as its name.

The event connection never carries PTY output and is not an attachment, follower,
viewer slot, or PTY-size participant. A recoverable reconnect replays only the
bounded contiguous event suffix after the accepted cursor. A missing, expired,
foreign-epoch, or invalid cursor instead replaces the host projection with the
daemon's reset snapshot. Activity is sampled as one `idle_ms`/`running` age pair
at that snapshot boundary; lifecycle, rename, and detected state remain
cursor-ordered facts.

Unread Done (`✓`) and NeedsAttention (`!`) come from the shared client attention
tracker, isolated per host and daemon epoch. They clear only after the exact
Snapshot's ghostty-web write callback and render-frame acknowledgement, while
the native window is focused. A background selected window still receives
notifications. The GUI notification lease holder dispatches metadata-only native
messages through `notification.rs` and `platform::notify`; desktop failures are
logged without clearing unread state. Initial/reset snapshots and lease grants
never replay old notifications.

All actor messages are generation-tagged and rejected before mutation after
host removal or reconnect. Snapshot rendering receipts bind the actor generation
and selection revision, while canonical names are resolved by exact identity;
rename during attach or between rendering and acknowledgement preserves Seen.

The local platform stream is a Unix socket on Linux/macOS and a named pipe on
Windows. Saved connections use `asd_proto::paths::data_dir()`, so their exact
path follows the platform contract rather than a hard-coded Unix location.
Native notification differences stay behind `src/platform/`; the host actor
must not perform platform notification I/O directly or let a delivery failure
roll back local Seen/unread state.

## Files

| File | Purpose |
|---|---|
| `src/lib.rs` | `run(session)` — Dioxus Desktop launch, embeds the vendor bundle + CSS |
| `src/app.rs` | `App` component, supervisor loop, settings overlay, connect menu |
| `src/conn.rs` | Per-host actor: control and replaying event connections, attach/detach, raw PTY bytes |
| `src/notification.rs` | Metadata-only native notification adapter and recording test adapter |
| `src/ssh.rs` | russh transport: known_hosts check, password/key auth, `asd attach --stdio` exec |
| `src/model.rs` | Hosts/sessions/selection model (+ unit tests) |
| `src/settings.rs` | Saved SSH connections, config persistence, form validation (+ unit tests) |
| `src/theme.rs` | One terminal palette shared by CSS variables, ghostty-web, and `Attach.appearance` |
| `src/bridge.rs` | `JsMessage` types + `include_str!` of the bridge script |
| `assets/bridge.js` | Terminal setup, fit-to-pane, JS→Rust events, `__asdWrite`/`__asdReset` |
| `assets/vendor-entry.js` | npm dependency roll-up: imports each package, assigns to `window` |
| `assets/app.css` | App stylesheet (the asd palette: dark bg, amber local / cyan remote rails) |
| `build.rs` | Drives `npm install` + `npm run build`, copies `dist/vendor.js` into `OUT_DIR` |
| `package.json` | npm dependency list + the esbuild bundling command |

## JS dependency workflow

npm owns the JS dependencies; esbuild bundles + minifies them into **one IIFE** (`dist/vendor.js`, WASM base64-inlined) which is `include_str!`-ed so the shipped `asd` stays a single self-contained binary.

To add a webview dependency:

```bash
cd crates/asd-dioxus
npm install <pkg>                  # updates package.json + lock
# then import it in assets/vendor-entry.js and assign to window
```

`build.rs` needs no changes — it just runs `npm install` + `npm run build` when the inputs change. Prerequisites: `node`/`npm` on PATH (use `.npmrc` for a registry mirror if needed).

## Bridge notes (Dioxus 0.8-alpha)

* **Rust→JS** uses `webview.evaluate_script()` calling `window.__asdWrite(...)` / `__asdReset()` — the eval bridge's `send()` is broken in this direction.
* **JS→Rust** uses `dioxus.send()`, but the *first* eval can race the page load and its channel goes deaf. Rust re-issues the eval until it hears something; every bridge run rebinds `window.__asdSend` to its own channel and all sends route through that binding, so the newest live channel carries the traffic.
* State that must survive re-renders (the AppCmd channel) lives in `use_hook` — a per-render channel drops its sender on the first re-render and silently unwinds the supervisor.
* Host actors run on a dedicated tokio runtime (`app.rs::bg`), keeping their timers/IO independent of the embedded runtime.

## Build & run

```bash
cargo build                        # workspace default = local + dioxus
cargo run                          # bare `asd` opens this GUI
cargo run -- gui demo              # preselect session "demo"
cargo test -p asd-dioxus           # model + settings unit tests
```

Linux needs WebKitGTK (`libwebkit2gtk-4.1-dev` to build, the runtime lib to run); macOS/Windows use the system webview. Servers that only need the daemon/CLI should build `--no-default-features` — no GUI libraries linked.

[dioxus]: https://dioxuslabs.com
[ghostty-web]: https://www.npmjs.com/package/ghostty-web
