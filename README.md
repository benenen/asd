# asd

[![CI](https://github.com/benenen/asd/actions/workflows/ci.yml/badge.svg)](https://github.com/benenen/asd/actions/workflows/ci.yml)

A terminal **session multiplexer**: a background daemon owns your PTY sessions,
reachable from a ratatui TUI, a scriptable CLI, and a GPU-accelerated desktop
GUI. Attach and detach at will — sessions and their scrollback survive the
client disconnecting. Local *and* remote-over-SSH sessions share one interface.

Unlike tmux, asd does **not** split panes or windows — one session is exactly
one PTY (closer in spirit to [shpool]). It spends that simplicity on a fast,
faithful terminal instead: exact scrollback replay, drag-to-copy selection, and
mouse-mode mirroring for full-screen apps like `vim`/`htop`.

[shpool]: https://github.com/shell-pool/shpool

## Features

- **Persistent sessions** — a background daemon owns each PTY; clients
  attach/detach freely and nothing is lost on disconnect.
- **Three clients, one binary** — `asd ui` (ratatui TUI), `asd attach` (a
  VT-rendering CLI client), and a GPU GUI (bare `asd`).
- **Local + remote** — reach a local daemon over a Unix socket, or a remote one
  over pure-Rust SSH (`russh`) with no `ssh` subprocess.
- **Scriptable** — `send` / `peek` / `wait` / `inspect` drive and observe
  sessions from scripts, no attach required.
- **Faithful terminal** — a local VT model per client: alternate-screen support,
  exact scrollback replay, drag-select → OSC 52 clipboard, and mouse-mode
  mirroring so `vim`/`htop` get real mouse events while the shell prompt keeps
  native copy.
- **Running/idle status** — each session reports whether its program is actively
  producing output; the TUI highlights running rows and `asd list` shows it.

## Architecture

asd ships as a **single `asd` binary** that combines the CLI, an embedded
daemon, the TUI, and the GUI (selected by Cargo features). The pieces are
library crates with hard dependency boundaries:

| Crate | Responsibility |
|---|---|
| `asd-proto` | Wire protocol — frame enum, `postcard` codec, framed reader/writer, path contract. |
| `asd-vt` | `VtBackend` trait + libghostty-vt implementation: the terminal model (cells, cursor, snapshot, key encoding). |
| `asd-daemon` | Session lifecycle + Unix-socket service — one PTY + headless terminal per session, broadcast to attached clients. |
| `asd-cli` | The `asd` command surface — the `attach` VT client, scripting commands, the embedded daemon, and the SSH `--stdio` proxy. |
| `asd-tui` | `asd ui` — a ratatui session sidebar next to a live terminal pane; switching, local scrollback, and selection. |
| `asd-dioxus` | The GPU GUI (Dioxus Desktop + ghostty-web): host-grouped sidebar, saved SSH connections, settings. |

**Daemon–client model:** a background daemon holds every session's PTY and
terminal state; clients connect over a Unix socket (or an SSH-proxied one) and
speak a length-prefixed `postcard` protocol. `Attach` replies with a full
`Snapshot`, then streams live `Output`. The daemon starts on demand — the first
`asd new` or `asd attach -A` spawns it.

## Install / build

Requires a recent Rust (edition 2024), **Zig 0.15.x** on `PATH` (it builds the
vendored `libghostty-vt-sys`), and — only for the GUI — **Node/npm** (bundles
the webview assets).

```bash
# Full build: CLI + daemon + TUI + GUI
cargo build --release
# → target/release/asd

# Server / headless: CLI + daemon + TUI, no GUI (won't link WebKitGTK)
cargo build --release --no-default-features --features local

# Client-only (e.g. Windows): GUI, no PTY/daemon
cargo build --release --no-default-features --features dioxus
```

Prebuilt binaries are produced by CI (currently green) for:

- Linux `x86_64` / `aarch64` — full
- Windows `x86_64-msvc` — GUI only
- macOS `x86_64` / `aarch64` — full

## Usage

The daemon starts automatically the first time you create or attach a session.

```bash
asd ui                       # open the TUI: sidebar + live pane (Ctrl+A prefix)
asd new [name] [--cmd CMD]   # create a session (auto-named s0, s1, …); default $SHELL
asd attach <name>            # attach a VT-rendering client (detach: Ctrl-\)
asd attach -A <name>         # attach, creating the session first if absent
asd list                     # list sessions: name, size, status, clients, command
asd kill <name>              # end a session (SIGHUP, then SIGKILL after 2s)
```

Scripting — drive and observe a session without attaching:

```bash
asd send build --text 'make test' --enter   # type into a session
asd send build --key C-c                     # named keys: Enter/Tab/Esc/arrows/C-a…C-z
asd peek build                               # print the rendered screen (--scrollback, --json)
asd wait build --text PASS --timeout 2m      # block until the screen contains "PASS" …
asd wait build --idle && asd peek build      # … or until output settles (2s), then read it
asd inspect build --json                     # full detail: pid, alt-screen, scrollback, mouse, cursor
```

Bare `asd` (or `asd gui [session]`) opens the desktop GUI.

### TUI keybindings

`asd ui` uses a `Ctrl+A` prefix (screen-style): press it, then a key.

- `j`/`k` or arrows — switch session; `1`–`9` — jump to session *N* (each sidebar row shows its matching ordinal prefix)
- `c` — new session
- `r` — rename the selected session (input modal; `Enter` confirms, `Esc` cancels; empty and duplicate names are rejected)
- `x` — kill the selected session (asks a `y`/`n` confirmation first)
- `b` — hide/show the sidebar (the pane goes full-width when hidden; showing it restores the current width)
- `R` — reconnect · `q` — quit · `Ctrl+A Ctrl+A` — send a literal `Ctrl+A` to the session

Mouse: click a sidebar row to switch (or its `x` to kill), drag in the pane to select (copied via OSC 52), and **drag the sidebar↔pane divider** to resize the sidebar (clamped to a sensible min/max). `Shift+PageUp`/`PageDown` page the scrollback.

### Remote SSH sessions

The GUI reaches remote daemons over SSH (pure-Rust `russh` — no `ssh`
subprocess; the far end runs `asd attach --stdio` to proxy its socket). Saved
connections live in `~/.local/share/asd/config.json`:

```json
{
  "ssh_connections": [
    {
      "name": "build box",
      "host": "build.example.com",
      "user": "me",
      "port": 22,
      "auth": { "method": "key", "key_path": "", "passphrase": "" }
    },
    {
      "name": "prod",
      "host": "10.0.0.9",
      "user": "ops",
      "auth": { "method": "password", "password": "hunter2" }
    }
  ]
}
```

`auth.method` is `key` (an empty `key_path` falls back to the default `~/.ssh`
keys; `passphrase` is optional) or `password`. Host keys are verified against
`~/.ssh/known_hosts` — unknown or changed keys are rejected. Secrets are stored
in plain text, the same trust model as `~/.ssh` on a single-user machine.

**Paths.** The daemon socket resolves as `$ASD_SOCKET` → `$XDG_RUNTIME_DIR/asd.sock`
→ `/tmp/asd-$UID/asd.sock`; config and logs live in `~/.local/share/asd/`.

## Recent highlights

- **Running/idle status + sidebar shimmer** — each session reports whether its
  program is producing output; the TUI sweeps a running row's text through a
  rainbow hue-shift (tachyonfx) and `asd list` gains a `STATUS` column.
- **Tear-free pane** — the pane defers a repaint while a program holds a
  synchronized-output update open (DEC mode `?2026`) and caches complete frames,
  so a rapidly self-redrawing TUI (e.g. an AI agent's status bar) is never
  sampled half-drawn.
- **Instant switching** — a session switch reveals the moment its exact two-pass
  snapshot is fed (~11 ms even for a 5,000-line scrollback); no resize jiggle or
  settle timers.
- **Native-feeling selection** — drag-to-select with a self-drawn highlight,
  copied via OSC 52; the selection is anchored in absolute screen space, so it
  tracks the text as you scroll.
- **Mouse-mode mirroring** — when a program (`vim`/`htop`) asks for the mouse,
  its exact DEC mouse modes are mirrored to the host so events pass through 1:1;
  otherwise the wheel scrolls and drags select locally.

## License

MIT — see [LICENSE](LICENSE).
