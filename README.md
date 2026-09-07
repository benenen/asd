# asd

[![CI](https://github.com/benenen/asd/actions/workflows/ci.yml/badge.svg)](https://github.com/benenen/asd/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/benenen/asd?label=release)](https://github.com/benenen/asd/releases/latest)
[![npm](https://img.shields.io/npm/v/@shibenenen/asd?label=npm)](https://www.npmjs.com/package/@shibenenen/asd)

**asd keeps terminal programs alive in named sessions and lets you return to
them from a TUI, a direct terminal client, a desktop GUI, or scripts.** The GUI
can also bring remote daemons into the same session list over SSH. The same
session model works for shells, long-running jobs, and coding agents.

asd is closer to [shpool] than tmux: one session owns exactly one PTY. It does
not split a session into panes or windows. Instead, it focuses on persistent
processes, faithful terminal rendering, fast session switching, and automation
that understands a terminal screen rather than scraping raw escape sequences.

[shpool]: https://github.com/shell-pool/shpool

## Why asd?

- **Persistent sessions** — detach or close a client without stopping the
  program running in the session.
- **One command, several clients** — use the desktop GUI, `asd ui`,
  `asd attach`, or attach-free CLI commands.
- **Faithful terminals** — alternate screens, exact scrollback replay,
  clipboard selection, bracketed paste, cursor state, and application mouse
  modes are modelled explicitly.
- **Local and remote hosts** — the GUI manages local sessions and remote daemons
  over pure-Rust SSH without spawning an `ssh` subprocess.
- **Automation without attaching** — send input, inspect a rendered screen,
  wait for output or agent state, and follow a session from scripts.
- **Agent-aware status** — the daemon classifies recognized coding-agent
  screens as `working`, `blocked`, `idle`, or `unknown`, so automation can
  distinguish a finished turn from a permission prompt.

## Install

If Node.js is already installed, the npm installer is the shortest path. Use a
release archive when you do not want Node on the machine; both install the same
full `asd` package.

### npm installer

The npm package downloads the matching binary from
[GitHub Releases](https://github.com/benenen/asd/releases/latest). Node.js 16 or
newer is required for the installer.

```bash
npm install -g @shibenenen/asd
asd --version

# Or run it without installing globally
npx @shibenenen/asd
```

The package name is scoped because the bare npm name is already taken; the
installed command is still `asd`.

### Release archives

You can also download an archive from
[GitHub Releases](https://github.com/benenen/asd/releases/latest):

| Platform | Target |
|---|---|
| Linux x64 | `x86_64-unknown-linux-gnu` |
| Linux arm64 | `aarch64-unknown-linux-gnu` |
| Windows x64 | `x86_64-pc-windows-msvc` |
| macOS Apple Silicon | `aarch64-apple-darwin` |

Windows archives contain both `asd.exe` and `ghostty-vt.dll`; keep them in the
same directory. Full Linux binaries use the WebKitGTK 4.1 runtime for the GUI.
Every prebuilt archive contains the full CLI, daemon, TUI, and GUI. For a
server without GUI libraries, use the [headless source build](#build-from-source).

## Quick start

Create a session in the current directory and open the terminal UI:

```bash
asd new work --cwd .
asd ui work
```

Inside `asd ui`, press `Ctrl+A`, then `q` to close the client. The session keeps
running. Open it again at any time:

```bash
asd ui work
```

You can also attach directly, without the sidebar:

```bash
asd attach work      # detach with Ctrl-\
```

Closing the desktop GUI also leaves its sessions running.

Create a session that starts a command immediately:

```bash
asd new server --cwd ./my-app --cmd 'npm run dev'
```

Inspect or stop it from another terminal:

```bash
asd list
asd peek server
asd kill server
```

The daemon starts automatically when `asd new`, `asd attach -A`, or `asd ui`
needs it.

## Choose a client

For a given host, every client speaks to the same daemon and sees the same
sessions. The GUI can show several local and remote hosts together.

| Command | Best for |
|---|---|
| `asd` or `asd gui [session]` | Desktop GUI with local and saved SSH hosts. |
| `asd ui [session]` | Terminal session switcher with a sidebar, live pane, and Git Graph. |
| `asd attach <session>` | A focused terminal client for one session. |
| `asd attach -r <session>` | Read-only watching without typing or resizing the session. |
| `asd send` / `peek` / `wait` / `follow` | Scripts and agents that should not attach interactively. |

Read-only mode prevents that attachment from accidentally typing or resizing;
it is not an access boundary, and `asd send` can still target the session.

Run `asd --help` or `asd <command> --help` for the complete command reference.
The TUI, direct attach client, and scripting commands address one daemon
endpoint; first-class multi-host SSH aggregation belongs to the desktop GUI.

## Common workflows

### Manage sessions

```bash
asd new                         # create s0, s1, ...
asd new build --cwd .           # create a named session in a directory
asd new web --cmd 'npm run dev' # start a command through the shell
asd list                        # list status, size, clients, and command
asd inspect build               # inspect terminal and process state
asd rename build release        # rename without disturbing the program
asd kill release                # end the session
```

`asd attach -A demo` starts the daemon and creates `demo` if either is missing.

### Drive a session from a script

```bash
asd send build --text 'make test' --enter
asd send build --key C-c
echo 'make test' | asd send build --enter

asd peek build
asd peek build --scrollback 200
asd wait build --text PASS --timeout 2m
asd wait build --idle
asd follow build
```

`send --enter` sends Enter as a separate keypress after the text, which matters
for full-screen TUIs that distinguish typing from paste bursts. `peek` returns
the rendered terminal screen rather than a stripped PTY byte stream.

For coding agents, `ask` and state-aware waiting provide a higher-level loop:

```bash
asd new agent --cwd . --cmd codex
asd ask agent 'run the tests and fix failures' # waits for idle or blocked
asd peek agent

# When specifically waiting for a turn to need human intervention
asd wait agent --until blocked --timeout 10m
```

`ask` refuses to type over an agent that is already waiting for an answer.
It prints whether the turn settled at `idle` or `blocked`; branch on that result
rather than assuming every turn needs intervention. `wait --until blocked` is
for a turn started elsewhere when only the human-input state matters. Built-in
screen classifiers cover Claude Code, Codex, OpenCode, and Pi.
See [Automation and observation semantics](docs/automation.md) for details on
`send`, `peek`, `wait`, `follow`, `card`, and modelled JSONL output.

### Address every session

```bash
asd send-all --key C-c --dry-run
asd send-all --text '/compact' --enter
```

The session running the command is skipped unless `--include-self` is set.
Use `--dry-run` before a broad action when you want to inspect its targets.

### Tell people and tools what a session is doing

Every session receives `ASD_SESSION` and `ASD_SOCKET`. A process inside the
session can publish a one-line status without naming itself:

```bash
asd status --text 'running integration tests'
asd status
asd status --clear
```

The line appears in `asd list`, `asd inspect`, and the TUI.
`ASD_SESSION` keeps the name assigned at spawn time; after `asd rename`, pass
the current session name explicitly when setting status.

## Terminal UI

`asd ui` uses a `Ctrl+A` prefix: press the prefix, release it, then press the
action key. The most common defaults are:

| Action | Default |
|---|---|
| Next / previous session | `Ctrl+Alt+Down` / `Ctrl+Alt+Up`, or `Ctrl+A j` / `Ctrl+A k` |
| Jump to session 1–9 | `Ctrl+A 1` … `Ctrl+A 9` |
| Create / rename / kill | `Ctrl+A c` / `Ctrl+A r` / `Ctrl+A x` |
| Hide sidebar / status bar | `Ctrl+A b` / `Ctrl+A s` |
| Open Git Graph | `Ctrl+A g` |
| Reconnect | `Ctrl+A R` |
| Page through scrollback | `Shift+PageUp` / `Shift+PageDown` |
| Send a literal `Ctrl+A` | `Ctrl+A Ctrl+A` |
| Quit the TUI | `Ctrl+A q` |

Git Graph opens for the selected session's repository. Use `j`/`k` or arrows
to move, `Tab`/`Shift+Tab` to change panes, `Enter` to open a changed file,
`/` to search, `[`/`]` to jump between refs, `?` for the full in-app key list,
and `q`/`Esc` to close the current layer. Repository detection currently needs
`/proc`, so Git Graph is Linux-only.

Mouse controls follow the region under the pointer:

- click a sidebar row to switch sessions, or its `x` to request a kill;
- drag the sidebar divider to resize it;
- drag terminal text to copy it through OSC 52, then right-click to paste;
- wheel over the sidebar, terminal pane, or Git Graph to scroll that region;
- when an application such as `vim`, `htop`, or a coding agent requests mouse
  tracking, its reports are forwarded one-for-one instead of scrolling local
  history.

Local scrolling is paced client-side so terminals that emit different numbers
of mouse reports feel consistent without changing application-owned mouse
input. Exact terminal invariants live in
[Terminal client behavior](docs/terminal-behavior.md).

Each session has one exclusive `asd ui` viewer. Selecting it from another TUI
transfers the view; ordinary `asd attach` clients remain shared.

All TUI bindings, including the leader, can be changed under `[keys]` in the
config file. See [`config.example.toml`](config.example.toml) for every action.

## Remote hosts

The desktop GUI can show local and remote daemons together. Add a host from
**Settings → Connections**. Install the same `asd` release on the remote
machine, ensure it is on `PATH` for non-interactive SSH commands, and start its
daemon by creating the first session:

```bash
# Run on the remote host
asd new work --cwd .
```

The GUI connects over SSH and runs `asd attach --stdio` on the far end to proxy
that daemon connection. Client and remote daemon protocol versions must match;
using the same release is the simplest way to guarantee it.

Authentication supports SSH keys, optional key passphrases, and passwords.
Host keys are verified against `~/.ssh/known_hosts`. On first connection the
GUI shows the unknown key for you to verify before using **Trust host key**;
changed keys remain rejected.

Saved connections and their secrets are stored as plain text in
`$XDG_DATA_HOME/asd/config.json` when set, otherwise
`~/.local/share/asd/config.json` on Linux/macOS, or in
`%LOCALAPPDATA%\asd\config.json` on Windows. Treat that file like other
single-user SSH configuration.

## Configuration

Configuration is optional and is never created automatically:

| Platform | Path |
|---|---|
| Linux / macOS | `~/.config/asd/config.toml` or `$XDG_CONFIG_HOME/asd/config.toml` |
| Windows | `%APPDATA%\asd\config.toml` |

`$ASD_CONFIG` overrides the path. A minimal file might be:

```toml
[session]
scrollback_lines = 10000
run_restored_commands = false

[keys]
leader = "Ctrl+A"

[keys.direct]
select_next = ["Ctrl+Alt+Down"]
select_previous = ["Ctrl+Alt+Up"]
```

The daemon reads `[session]`; `asd ui` reads `[keys]`; each ignores the other's
table. Copy [`config.example.toml`](config.example.toml) for the complete,
commented set of options.

Agent-detection rules can be replaced per agent under `agents/` beside
`config.toml`. See
[Agent state](docs/architecture.md#agent-state) for the rule ownership model.

Configuration is read once at startup. Reopen `asd ui` after changing `[keys]`.
Changing `[session]` requires a daemon restart, so read the warning below first.

## Persistence, upgrades, and restart

The daemon owns the running processes. Replacing or upgrading the `asd` binary
does not replace a daemon that is already running.

`asd restart` starts the new daemon and recreates saved session names, working
directories, and start commands. **The live programs and their screen contents
do not survive.** Restored commands are typed at fresh shell prompts but are
not executed unless `run_restored_commands = true`.

Client and daemon protocol versions must match. After an upgrade that changes
the protocol, finish or otherwise preserve important work before restarting.
The persistence model, paths, and on-disk record are documented in
[Architecture](docs/architecture.md#session-lifecycle-and-persistence).

## Troubleshooting

`asd inspect <session>` reports the live PID, screen mode, scrollback depth,
mouse modes, and cursor. For daemon logs, set `RUST_LOG=debug` before starting
it. A foreground `asd daemon` logs to the terminal; an automatically started
daemon appends to:

| Platform | Log |
|---|---|
| Linux / macOS | `~/.local/share/asd/daemon.log` or `$XDG_DATA_HOME/asd/daemon.log` |
| Windows | `%LOCALAPPDATA%\asd\daemon.log` |

## How it works

```text
GUI / asd ui / asd attach / scripts
                 │
        postcard protocol
                 │
              daemon
                 │
        one PTY per session
```

The daemon is the authority for each session's PTY, terminal state, activity,
and agent state. An attaching client first receives a full terminal snapshot,
then ordered live output. Local clients connect through a Unix socket or
Windows Named Pipe; the GUI can carry the same protocol through SSH.

Contributor-facing crate boundaries, threading, flow control, and protocol
contracts are in [Architecture](docs/architecture.md).

## Build from source

Source builds require a Rust toolchain with edition 2024 support and
**Zig 0.15.x** on `PATH` for the vendored `libghostty-vt`.

The default build includes the desktop GUI and also requires Node/npm. Linux
needs the WebKitGTK development libraries; Windows needs NASM.

```bash
# Debian / Ubuntu GUI dependencies
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libxdo-dev

# Full binary: CLI + daemon + TUI + GUI
cargo build --locked --release
```

For a server binary with the CLI, embedded daemon, and TUI but no GUI:

```bash
cargo build --locked --release --no-default-features
```

On Unix, install the completed build with:

```bash
sudo install -m 0755 target/release/asd /usr/local/bin/asd
```

A Windows source build also produces `ghostty-vt.dll`; copy it beside
`target\release\asd.exe` before running the executable.

## Documentation

- `asd --help` and `asd <command> --help` — command syntax and options.
- [`config.example.toml`](config.example.toml) — all daemon and TUI settings.
- [Automation and observation semantics](docs/automation.md) — non-attached
  clients, rendered output, activity, agent state, and `card`.
- [Terminal client behavior](docs/terminal-behavior.md) — snapshots, colors,
  mouse, clipboard, paste, resizing, and repaint behavior.
- [Architecture](docs/architecture.md) — crates, protocol flow, lifecycle,
  persistence, paths, and GUI boundaries.
- [Cross-platform development](docs/cross-platform-development.md) — build and
  verification notes for Linux, macOS, and Windows.
- [Desktop GUI internals](crates/asd-dioxus/README.md) — actor and WebView
  architecture.

The documentation index is [`docs/README.md`](docs/README.md).

## License

MIT — see [LICENSE](LICENSE).
