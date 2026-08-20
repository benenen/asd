# Architecture

This document records the internal ownership and dependency contracts that are
not obvious from a directory listing. User-facing commands and configuration
examples remain in the repository [`README`](../README.md).

## Product and binary composition

`asd` is a shpool-style session multiplexer: one session is one PTY, with no
tmux-style pane or window hierarchy. A headless daemon owns the live child
processes, PTYs, terminal state, and scrollback. Clients can disappear without
ending a session.

Distribution is intentionally a single executable. The workspace root is both
`[workspace]` and `[package]`; `src/main.rs` combines library crates through
features:

- `asd-cli`, `asd-tui`, `asd-daemon`, and portable-pty are unconditional.
- `dioxus` brings in `asd-dioxus`, Dioxus Desktop, and ghostty-web.
- `gui` is a compatibility alias for `dioxus`.
- `dioxus` is the default and the only feature; `--no-default-features`
  yields the headless-server binary.

The root package contains composition and dispatch, not duplicated business
logic. The daemon is still launched as `asd daemon`, including self-healing
re-exec through `current_exe()`. `asd-cli` receives the optional GUI launcher
from the root binary, so the CLI library never imports a GUI framework.

## Crate boundaries

| Crate | Responsibility | Hard boundary |
|---|---|---|
| `asd-proto` | Shared frame enum, postcard codec, framed reader/writer, and path contract | No business crate; no runtime other than Tokio |
| `asd-client` | Client-side handshake and attach convergence shared by CLI, TUI, and GUI | No GUI, portable-pty, process management, or `asd-vt` |
| `asd-vt` | `VtBackend`, libghostty-vt adapter, render snapshots, terminal helpers | No GUI, portable-pty, or protocol dependency |
| `asd-daemon` | PTY/session ownership, terminal state, registry, local service | No GUI dependency, including transitive GUI frameworks |
| `asd-cli` | CLI commands, VT attach client, embedded daemon, `--stdio` proxy | GUI launcher is injected; no GUI framework |
| `asd-tui` | ratatui `asd ui`, sidebar, terminal pane, scrollback, selection | No GUI framework or PTY/process management |
| `asd-dioxus` | Desktop UI, ghostty-web renderer, saved hosts, pure-Rust SSH | No portable-pty or local process management |
| root `asd` | Feature composition and command dispatch | No direct implementation logic from the two feature families |

Platform differences live behind `src/platform/{unix,win}.rs` in each relevant
crate. `platform/mod.rs` mounts exactly one implementation as `imp` and
explicitly re-exports a shared interface, letting the compiler catch platform
drift. Call sites remain free of Unix/Windows `cfg` branches. Foreground command
lookup is the existing exception because it has separate Linux, macOS, and
Windows implementations rather than a Unix/Windows split.

## Session threading and ordering

Network I/O is asynchronous Tokio work. Each session adds two standard threads:

1. a blocking PTY reader sends byte batches to the session channel;
2. a session thread exclusively owns the `GhosttyVt`, child handle, attached
   clients, followers, and size state.

PTY output, attach/detach, input, resize, inspect, history, and scripted input
all enter that same `std::sync::mpsc` channel. This serialization is the
ordering primitive: a successful attach enqueues its Snapshot before any later
Output can be broadcast.

`GhosttyVt` is deliberately `!Send` and never leaves its session thread. The
daemon terminal is also the only side allowed to answer DA/DSR and OSC color
queries to the child PTY. Rendering clients may maintain local terminal models,
but their generated application replies are drained and discarded.

## Connection data path and flow control

`crates/asd-daemon/src/conn.rs` splits each connection into an inbound reader
and outbound writer task. `FrameReader::read_frame` is not cancellation-safe,
so it must not be placed directly in a `tokio::select!`. Control replies and
session broadcasts share one outbound queue, preserving their per-connection
order.

Outbound Snapshot and Output payloads consume the `ClientSink` quota. The
accounting helper also recognizes Input defensively, but Input is inbound and
bypasses `ClientSink` in the normal path. The cap is 4 MiB per client.
Exceeding it queues a connection close and removes that client without
disturbing the session. Every removal path, including a failed direct reply,
must remove client membership and its `client_sizes` entry, release any TUI
viewer ownership, and recompute PTY size. Leaving a dead small client in size
negotiation permanently shrinks the shared terminal.

The wire format is `u32` little-endian payload length followed by postcard,
with a 4 MiB frame cap. The client sends `Hello`; unequal protocol versions
receive the version-mismatch error and disconnect. Frame definitions, the
current version, and the complete version history live in
[`asd-proto/src/lib.rs`](../crates/asd-proto/src/lib.rs).

## Session membership

Ordinary CLI attach and desktop GUI connections are shared: all may view and
type into the same session. `asd ui` adds a single exclusive interactive-view
slot per session. Replacing that TUI viewer does not evict shared clients. The
detailed `view_id`, rename, revoke, and convergence rules are in
[`terminal-behavior.md`](terminal-behavior.md).

Followers are a separate collection. They receive live Output plus
`FollowStatus`, but no Snapshot. They do not count in `attached_clients`, do not
participate in size negotiation, and cannot send ordinary attached-client
operations.

Input messages carry the originating client ID. The session thread validates
membership for input, resize, history, and refresh operations so a disconnected
or revoked client loses capability immediately rather than relying on UI
cooperation.

## Live session metadata

`SessionInfo.command` reports the current foreground job rather than only the
spawn command. On Linux, the daemon reads the PTY foreground process group and
then `/proc/<pgid>/cmdline`; on macOS it parses `KERN_PROCARGS2` with a libproc
path fallback. Parsing strips common `sh -c` wrappers and falls back to the
spawn command or default shell when process inspection is unavailable. The PTY
master descriptor is borrowed for foreground-group lookup rather than
duplicated, because an extra master lifetime would prevent slave hangup.

Terminal title comes from OSC 0/2 observed by the daemon VT. Activity uses one
`last_output_ms` timestamp and the shared `IDLE_SETTLE_MS` threshold, so list
status, `wait --idle`, follow status, and TUI shimmer cannot develop separate
definitions of running.

## Session lifecycle and persistence

Connection loss implicitly detaches its membership. PTY EOF is the session
terminal condition: reap the child, remove the registry entry, and notify
attached clients that the session exited. `Kill` sends SIGHUP and uses SIGKILL
after two seconds if needed. Daemon shutdown applies the same graceful-then-hard
sequence before removing its endpoint.

Live processes, terminal cells, and scrollback are not persisted. Session name
and working directory are. Registry create, rename, and removal rewrite
`sessions.tsv` atomically through a temporary file plus rename. Daemon startup
recreates each saved entry as a fresh default shell in its recorded directory.
An ordinary kill or shell exit removes the saved entry; a daemon restart or
crash restores it.

Before intentional daemon shutdown, `freeze_and_persist` captures live working
directories and freezes persistence. Otherwise the subsequent SIGHUP-driven
session cleanup would rewrite the file as an empty list. If a process working
directory cannot be read, persistence falls back to the daemon's default
directory.

The session list belongs to the data directory, not to the socket name. Setting
only `ASD_SOCKET` does not isolate an experimental daemon's persistence; set a
separate data directory as well.

## Paths and configuration

All daemon/client endpoint resolution lives in `asd_proto::paths` so both ends
use exactly the same contract.

- Unix socket priority: `ASD_SOCKET`, then
  `$XDG_RUNTIME_DIR/asd.sock`, then `/tmp/asd-$UID/asd.sock` with a protected
  parent directory.
- Windows endpoint priority: `ASD_SOCKET`, then
  `\\.\pipe\asd-<USERNAME>`.
- Unix data defaults to `~/.local/share/asd`; Windows data defaults to
  `%LOCALAPPDATA%\asd`.
- Each session's child process is spawned with `ASD_SESSION` (its name at spawn
  time) and `ASD_SOCKET` (the listener the hosting daemon actually serves, which
  is not necessarily what `paths::socket_path` would resolve). The daemon owns
  both; `spawn_session` receives the socket from the registry rather than
  re-resolving it.
- Unix config defaults to `~/.config/asd/config.toml`; Windows config defaults
  to `%APPDATA%\asd\config.toml`; `ASD_CONFIG` overrides both.

On Windows, the PID file cannot be derived by applying a file extension to a
named-pipe path. It lives in the data directory and includes the pipe name so
custom endpoints remain isolated. The first pipe instance must request
`first_pipe_instance(true)`; otherwise two daemons can share one name and
randomly split clients between unrelated registries.

Configuration is read once at daemon startup and is never auto-created.
Missing or malformed files fall back to defaults so a bad optional config
cannot prevent startup. `RawConfig` fields remain optional with serde defaults;
unknown keys are ignored for forward/backward tolerance. Each spawned or
restored session receives the registry's configured scrollback line count.

## Desktop GUI

The GUI uses ghostty-web directly and therefore does not use `asd-vt` for its
rendered terminal. It also cannot start a local daemon because that would cross
the PTY/process-management boundary. Local and remote hosts share one actor
protocol; remote transport is pure-Rust `russh` executing
`asd attach --stdio` on the far end.

The actor model, pending-attach protection, webview bridge, and JavaScript
bundle are documented in
[`crates/asd-dioxus/README.md`](../crates/asd-dioxus/README.md). Keep those
details there rather than duplicating them in repository-wide guidance.
