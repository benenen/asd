# asd

[![CI](https://github.com/benenen/asd/actions/workflows/ci.yml/badge.svg)](https://github.com/benenen/asd/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/benenen/asd?label=release)](https://github.com/benenen/asd/releases/latest)
[![npm](https://img.shields.io/npm/v/@shibenenen/asd?label=npm)](https://www.npmjs.com/package/@shibenenen/asd)

A terminal **session multiplexer**: a background daemon owns your PTY sessions,
reachable from a ratatui TUI, a scriptable CLI, and a desktop
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
  VT-rendering CLI client), and a desktop GUI (bare `asd`).
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
- **Agent state** — for a recognized coding agent the daemon also reads its
  *screen*, so a session that stopped to ask you something reports `blocked`
  rather than merely "not producing output". `asd wait --until blocked` returns
  on it, and both the TUI and the GUI mark the row.

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
| `asd-dioxus` | The desktop GUI (Dioxus Desktop + ghostty-web): host-grouped sidebar, saved SSH connections, settings. |

**Daemon–client model:** a background daemon holds every session's PTY and
terminal state; clients connect over a Unix socket (or an SSH-proxied one) and
speak a length-prefixed `postcard` protocol. `Attach` replies with a full
`Snapshot`, then streams live `Output`. The daemon starts on demand — the first
`asd new` or `asd attach -A` spawns it.

Contributor-facing ownership, protocol, terminal, and cross-platform contracts
are indexed in [`docs/`](docs/README.md).

## Install / build

### Install a prebuilt release

The npm installer downloads the matching binary from
[GitHub Releases](https://github.com/benenen/asd/releases/latest) and exposes it
as `asd` (Node.js 16 or newer is required for the installer):

```bash
npm install -g @shibenenen/asd
asd --version

# Or run it once without installing it globally
npx @shibenenen/asd
```

You can also download and unpack a release archive directly. Prebuilt full
binaries (CLI + daemon + TUI + GUI) are published for:

| Platform | Release target |
|---|---|
| Linux x64 | `x86_64-unknown-linux-gnu` |
| Linux arm64 | `aarch64-unknown-linux-gnu` |
| Windows x64 | `x86_64-pc-windows-msvc` |
| macOS Apple Silicon | `aarch64-apple-darwin` |

The Windows zip holds **two** files that belong together: `asd.exe` and
`ghostty-vt.dll`. Keep them in the same directory — the exe imports the DLL and
Windows will refuse to start it otherwise. Linux release binaries require the
WebKitGTK 4.1 runtime; use the headless source build below on servers where GUI
libraries are unavailable.

Upgrading the executable does not replace a daemon that is already running.
`asd restart` starts the new daemon, but recreates saved sessions as fresh
shells; their current processes and screens do not survive the restart.

### Build from source

All builds require a Rust toolchain with edition 2024 support and **Zig 0.15.x**
on `PATH` for the vendored `libghostty-vt`. The default full build also requires
**Node/npm**; on Linux it needs the WebKitGTK development libraries, and on
Windows it needs NASM. For Debian/Ubuntu:

```bash
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libxdo-dev
```

Build the default full binary:

```bash
cargo build --locked --release
# -> target/release/asd (target/release/asd.exe on Windows)
```

A Windows source build also leaves `ghostty-vt.dll` under Cargo's build output;
copy it beside `target\release\asd.exe` before running the program. The release
zip and npm installer stage this DLL automatically.

For a server/headless binary with the CLI, embedded daemon, and TUI but no GUI
or WebKitGTK dependency:

```bash
cargo build --locked --release --no-default-features
```

On Unix, install either completed build on `PATH` with:

```bash
sudo install -m 0755 target/release/asd /usr/local/bin/asd
```

## Usage

The daemon starts automatically the first time you create or attach a session.

```bash
asd ui                       # open the TUI: sidebar + live pane (Ctrl+A prefix)
asd new [name] [--cmd CMD]   # create a session (auto-named s0, s1, …); default $SHELL
asd attach <name>            # attach a VT-rendering client (detach: Ctrl-\)
asd attach -A <name>         # attach, creating the session first if absent
asd attach -r <name>         # watch read-only: keys go nowhere, size unaffected
asd status [name] --text …   # say what this session is doing (from inside it)
asd list                     # list sessions: name, size, status, clients, command
asd kill <name>              # end a session (SIGHUP, then SIGKILL after 2s)
```

Scripting — drive and observe a session without attaching:

```bash
asd ask build 'make test'                   # type, then wait for it to settle
asd send build --text 'make test' --enter   # type into a session
asd send build --key C-c                     # named keys: Enter/Tab/Esc/arrows/C-a…C-z
asd send-all --text '/compact' --enter        # … or type it into every session at once
asd send-all --key C-c --dry-run             # (--dry-run lists the targets first; your own
                                             #   session is skipped unless --include-self)
echo 'make test' | asd send build --enter    # or pipe it in (--enter folds the shell's newline
                                             #   away, then sends Enter separately so a TUI submits)
asd peek build                               # print the rendered screen (--json)
asd peek build --scrollback                  # … with all its history above it
asd peek build --scrollback 200              # … or just the last 200 lines of it
asd wait build --text PASS --timeout 2m      # block until the screen contains "PASS" …
asd wait build --idle && asd peek build      # … or until output settles (2s), then read it
asd wait agent --until blocked               # … or until a recognized agent stops to ask you something
asd follow build                             # stream output live, return when it settles
asd follow build --forever                   # … or keep streaming until the session ends
asd follow build --json                      # … as JSONL: one event object per line
asd inspect build --json                     # full detail: pid, alt-screen, scrollback, mouse, cursor
```

Which session should a task run in? `asd card` answers from the project itself
— the documents in each session's working directory:

```bash
asd card                            # one line per session: where it is, which docs it has
asd card list --json                # … for a program to choose from
asd card inspect build --json       # that session's card: each doc's heading + opening lines
asd card cat build AGENTS.md        # one file in full (any path under the session's directory)
```

`list` → `inspect` → `cat` is a deliberate ladder: choosing a session usually
only needs the first, so an agent does not pull three READMEs into its context
to pick one. The set of documents is fixed — `README.md`, `CLAUDE.md`,
`AGENTS.md`, `CONTRIBUTING.md`, matched **ignoring case**, so a project
spelling it `readme.md` still has a card — and `cat` reaches any file under the
directory, matching its path the same way, refusing paths that leave it.

`card` works against a **local** daemon: a session's directory is read from its
own process (`/proc/<pid>/cwd`), so for a session on a remote daemon the card
reports the directory as unknown rather than guessing at a local pid.

`follow` is `wait --idle` that keeps the output instead of discarding it — for
watching an agent (Claude Code, Codex) work through a task, where there is no
string worth matching on because the screen is redrawn continuously. It ends on
the daemon's own quiescence signal, delivered inline with the bytes rather than
polled, so "here is the output, and now it is done" arrives in that order.

`--json` makes it JSONL, so a program can consume the same stream:

```jsonl
{"event":"status","time_ms":1785290443950,"running":true,"idle_ms":1}
{"event":"output","time_ms":1785290445242,"text":"LINE-2"}
{"event":"output","time_ms":1785290445293,"text":"LINE-3"}
{"event":"screen","time_ms":1785290450682,"text":"LINE-88\nLINE-89\nLINE-90\n$ "}
{"event":"status","time_ms":1785290450682,"running":false,"idle_ms":2000}
```

This is modelled, not echoed. The bytes go through a terminal (the same one
`attach` renders with), which splits them in two:

- **`output`** — lines that have scrolled off the live screen. A row that has
  left the screen can never be rewritten, so its content is final: logged in
  order, exactly once, as plain text.
- **`screen`** — the live screen at each pause (settle, session end, timeout).
  This is the part a program repaints, so it is reported once per pause however
  many times it was drawn.

That distinction is the whole point. A TUI rewrites its status line several
times a second — `✻ building…`, `✽ building… 2`, `· building…` — and in the
byte stream those are indistinguishable from new output; stripping escape
sequences does not help, because the escapes *are* the distinction. A terminal
knows, because it has row identity.

Two consequences: output that never scrolls (a short command on a screen with
room to spare) is not final until the session settles, so it arrives in
`screen` rather than streaming line by line; and a full-screen program on the
alternate screen (vim, htop, less) commits nothing by design — its screen *is*
the content. `--raw` skips the model entirely and reports the verbatim stream,
escapes and repaints included.

`status` is logged only when the session's activity flips, since the daemon
reports it after every batch. The stream ends with `exit` when the session
does, or `timeout` when `--timeout` expires (exit code 4).

Bare `asd` (or `asd gui [session]`) opens the desktop GUI.

### TUI keybindings

These are the current defaults. `asd ui` uses a `Ctrl+A` prefix
(screen-style): press it, then a key. Runtime hints and key dispatch are both
generated from the same keymap registry so they stay in sync.

`Ctrl+Alt+↓`/`Ctrl+Alt+↑` walk the session list on their own, without the
prefix — the one move frequent enough to be worth its own chord. Everything
else is behind the prefix:

- `j`/`k` or arrows — switch session; `1`–`9` — jump to session *N* (each sidebar row shows its matching ordinal prefix)
- `c` — new session
- `r` — rename the selected session (input modal; `Enter` confirms, `Esc` cancels; empty and duplicate names are rejected)
- `x` — kill the selected session (asks a `y`/`n` confirmation first)
- `b` — hide/show the sidebar (the pane goes full-width when hidden; showing it restores the current width)
- `s` — hide/show the bottom status bar, which besides the clock shows the
  daemon host's CPU, memory, and network throughput (each behind its own icon,
  dropping right to left as the terminal narrows)
- `g` — open the git graph for the focused session's repository (`j`/`k` move,
  `Ctrl+d`/`Ctrl+u` page, `g`/`G` jump to newest/oldest, `@` returns to HEAD,
  `R` re-reads the repository, `y` copies the selected hash, `q` or `Esc`
  closes). The prefix reaches through it, so `Ctrl+A 1` switches session and
  re-aims the graph — and `Ctrl+A Ctrl+A` still sends a literal `Ctrl+A` to
  the session *behind* the overlay, since the whole leader sequence is passed
  through. Finding a session's directory needs `/proc`, so this is Linux-only
  for now
- `R` — reconnect · `q` — quit · `Ctrl+A Ctrl+A` — send a literal `Ctrl+A` to the session

Mouse: click a sidebar row to switch (or its `x` to kill), drag in the pane to select (copied via OSC 52), and **drag the sidebar↔pane divider** to resize the sidebar (clamped to a sensible min/max). `Shift+PageUp`/`PageDown` page the scrollback.

All of it is rebindable, leader included, under `[keys]` in the config file
below — worth knowing if your desktop takes `Ctrl+Alt`+arrows for switching
workspaces before the terminal ever sees them. Bindings that will not compile
leave every default in place and say why on the notice line, so a typo cannot
lock you out of your sessions.

Each session can be shown by only one `asd ui` at a time. Selecting a session
in another TUI transfers the view to that TUI; the displaced TUI stays open,
shows an ASD placard, and can select the session again to take it back. This
only governs TUI rendering: ordinary `asd attach` clients remain shared and
continue to view and control the same session.

**Watching without touching.** `asd attach --read-only` (`-r`) attaches a client
that receives everything and sends nothing: the daemon drops its keystrokes
instead of writing them to the pty, and leaves it out of size negotiation, so
opening a watcher in a narrow window will not reflow the session under whoever
is working in it. Useful for looking over an agent's shoulder without being one
stray keystroke away from derailing it. It is a guard against accidents, not an
access boundary — that same terminal can still run `asd send`, exactly as a
`tmux attach -r` client can still run `tmux send-keys`.

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
→ `/tmp/asd-$UID/asd.sock`; the GUI's `config.json`, the daemon log, and the
session list below live in the data directory, `~/.local/share/asd/`.

**Session environment.** Every session's program is started with two variables
of its own, so a script or agent running *inside* a session can address the
session it lives in:

| | |
|---|---|
| `ASD_SESSION` | the session's name at spawn time (a later rename does not update it) |
| `ASD_SOCKET` | the socket of the daemon hosting it — the exact one it serves, not the default the resolution order would pick |

`ASD_SOCKET` is what makes `asd list` / `asd new` inside a session answer for
that session's daemon even when it was started with `--socket`. Both follow the
same precedence as everywhere else, so exporting your own value overrides them.

### Configuration

The config file is optional, read-only, and never auto-created — a missing file
simply means "all defaults". Two readers share it and ignore each other's
tables: the daemon reads `[session]`, `asd ui` reads `[keys]`.

| | |
|---|---|
| Linux/macOS | `~/.config/asd/config.toml` (`$XDG_CONFIG_HOME/asd/config.toml` when set) |
| Windows | `%APPDATA%\asd\config.toml` |

`$ASD_CONFIG` overrides that path entirely. Note it is the *config* directory,
deliberately apart from the data directory above, so a file you hand-edit never
sits among the ones the daemon rewrites. (It is also not the GUI's
`config.json` of saved SSH connections, which is machine-local state and does
live in the data directory.)

```toml
[session]
# Lines of scrollback history each session's terminal keeps — how far you can
# scroll back, and how much `asd peek --scrollback` can return. Default 10000;
# 0 disables scrollback. Memory grows only with the lines actually produced.
scrollback_lines = 10000

# Whether a restored session's recorded command runs by itself. Default false:
# a restart types the command at the restored shell's prompt and waits for you
# to press Enter. `asd daemon --run-restored-commands` forces it on for one
# daemon.
run_restored_commands = false

[keys]
# The TUI's prefix key. Press it, then a `[keys.prefix]` key. Default "Ctrl+A".
leader = "Ctrl+A"

[keys.direct]
# Bindings that fire without the prefix, so they have to be chords the session
# would never want. Default Ctrl+Alt+↓ / Ctrl+Alt+↑ for the session list.
select_next = ["Ctrl+Alt+Down"]
select_previous = ["Ctrl+Alt+Up"]

[keys.prefix]
# Bindings behind the leader, where a bare letter is free to mean something.
# Each entry lists *all* the keys for that action, replacing the defaults.
select_next = ["j", "Down"]
quit = ["q"]
```

Every key is optional and unknown keys are
ignored, so a partial file merges onto the defaults and an older daemon
tolerates a newer file. A malformed one is not fatal either — the daemon logs a
warning and serves with defaults rather than refusing to start, and `asd ui`
falls back to its default bindings with the reason on the notice line.

The file is read **once at startup** by each of its readers, so run `asd
restart` after editing `[session]` and reopen `asd ui` after editing `[keys]`.
`config.example.toml` in the repository root is a ready-to-copy template that
lists every action by name.

### Agent state

Alongside `running` (is output arriving?), the daemon reports what the program
on a session's screen is *doing*, where it recognizes one: `working`, `blocked`,
`idle`, or `unknown`. The two answer different questions — an agent stopped at
a permission prompt is not producing output, so activity alone calls it "idle",
which is the one word that most misdescribes a session waiting on a person.

Rules live in TOML, one file per agent, shipped inside the binary for `claude`,
`codex`, `opencode`, and `pi`. To change one — an agent's UI moved, or you want
a rule of your own — drop a file with the same `id` in:

| | |
|---|---|
| Linux/macOS | `~/.config/asd/agents/<agent>.toml` |
| Windows | `%APPDATA%\asd\agents\<agent>.toml` |

A user file **replaces** the built-in rules for that agent rather than merging
with them, so a rule that has started firing wrongly can be removed and not
just outvoted. An unparsable file is skipped with a warning and the built-in
rules stand; a file declaring a `min_engine_version` this daemon does not
implement is skipped too.

A rule names a region of the screen (`osc_title`, `bottom_non_empty_lines(N)`,
or `whole_screen`), the conditions to look for there, and the state a match
means. Highest `priority` wins.

```toml
id = "claude"
min_engine_version = 1

[[rules]]
id = "title_spinner"
state = "working"
priority = 1100
region = "osc_title"
line = [{ first_char_in = ["2800-28ff", "25d0-25d3"] }]

[[rules]]
id = "permission_prompt"
state = "blocked"
priority = 980
region = "bottom_non_empty_lines(16)"
contains = ["do you want to proceed?"]
```

Each entry in `line` must be satisfied by a *single* line (`starts_with`,
`first_char_in`, `contains`); `contains` at the rule level may match anywhere in
the region; `any` and `not` nest. A rule with no conditions matches nothing, so
a misspelled key cannot pin every session to one state. `RUST_LOG=debug` makes
the daemon log which rule produced each state change.

### Session persistence

Session **names, working directories, and start commands** outlive the daemon;
the live processes and screens do not. The daemon keeps them in a tab-separated file, rewritten
atomically (temp file + `rename`) on every create/rename/kill and read back on
every startup:

| | |
|---|---|
| Linux/macOS | `~/.local/share/asd/sessions.tsv` (`$XDG_DATA_HOME/asd/sessions.tsv` when set) |
| Windows | `%LOCALAPPDATA%\asd\sessions.tsv` |

One line per session, `name` TAB `cwd` TAB `command`:

```tsv
web	/home/me/proj	npm run dev
```

Either trailing field may be empty, so a plain shell session's line ends in a
tab:

| name | cwd | command |
|---|---|---|
| `web` | `/home/me/proj` | `npm run dev` |
| `logs` | `/var/log` | *(empty: a plain shell)* |
| `s0` | *(empty: cwd unreadable)* | *(empty)* |

The cwd is empty when it could not be read — as on macOS, or for a session whose
process is already gone (`s0` above); those come back in the daemon's default
directory. The command is empty for a session started as a plain shell, which is
most of them. Blank and nameless lines are skipped on read. Session names are
`[A-Za-z0-9_-]` and paths don't contain tabs in practice, so those two fields are
written as-is; a command is arbitrary text, so a tab or newline in it is escaped
(`\t`, `\n`, `\r`, `\\`) and nothing else is. The file is meant to be read, not
hand-edited.

On the next startup — `asd restart`, a reboot, or a crash — every entry is
recreated as a fresh `$SHELL` in its saved directory. Killing a session or
letting its shell exit rewrites the file without it, so it stays gone.

**A recorded command comes back typed, not run.** The restored session is a
shell with the command sitting on its prompt line, so you can press Enter to run
it, edit it first, or Ctrl+C to drop it. asd will not re-run an arbitrary
command on its own — a restart is not a reason to deploy again — so the
confirmation is yours to give:

```console
$ asd new build --cmd 'cargo build --release && ./deploy.sh'
$ asd restart
$ asd attach build
me@host:~/proj$ cargo build --release && ./deploy.sh▌   # waiting for Enter
```

The record belongs to the session, not to one boot: a session created with
`--cmd` stages that command on every restart until the session is killed.

To skip the confirmation, start the daemon with `asd daemon
--run-restored-commands`, or set `session.run_restored_commands = true` in the
config file so every daemon does it.

The list belongs to the **data directory, not the socket**: `ASD_SOCKET` alone
does not isolate it, so a second daemon run for experiments wants
`XDG_DATA_HOME` pointed elsewhere too, or it will rewrite the real list.

### Logs

The daemon logs to **stderr** at `info` level; `RUST_LOG` sets the filter
(`RUST_LOG=debug`, `RUST_LOG=asd_daemon=trace`, …). Where that stderr lands
depends on who started it:

- **Auto-spawned** — `asd new`, `asd attach -A`, and `asd ui` start the daemon
  detached, with its stdout and stderr redirected to `daemon.log` in the data
  directory: `~/.local/share/asd/daemon.log`, or
  `%LOCALAPPDATA%\asd\daemon.log` on Windows. Set `RUST_LOG` in the
  environment of whichever command spawns it, since the daemon inherits it.
- **Foreground** — `asd daemon` writes to your terminal and touches no file.

The log is opened for **append and never rotated or truncated**, so it grows
for as long as you keep using asd; deleting it is safe at any time, and the
next spawn recreates it. Like the session list it is keyed to the data
directory rather than the socket, so daemons started on different
`--socket`/`$ASD_SOCKET` paths all append to this one file.

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
- **Pastes stay pastes** — multi-line text pasted into `asd ui` or `asd attach`
  reaches the session bracketed (DEC 2004) whenever the program asked for it, so
  the line breaks in it are text rather than a series of Enters running every
  line above the last.

## License

MIT — see [LICENSE](LICENSE).
