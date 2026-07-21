# `asd restart` preserves each session's workspace (cwd)

Date: 2026-07-21
Status: proposed (awaiting review)

## Problem

`asd restart` today stops the daemon and starts a fresh one; because sessions
are not persisted, every session (and its running program) is lost and the new
daemon comes up empty. The daemon even documents this in `serve()`:
"Sessions are not persisted: restarting the daemon = all sessions gone."

The user wants `asd restart` to bring the sessions back — **not** the live
processes or the on-screen content, only each session's **working directory**:
after a restart, every session exists again (same name) as a **fresh default
shell already `cd`'d to the directory it was in** before the restart.

## Goals

- `asd restart` recreates every pre-restart session by name, each as a fresh
  `$SHELL` whose working directory is the session's pre-restart cwd.

## Non-goals

- Keeping the live child processes alive (codex/claude/etc. are **not** brought
  back — the recreated session is a clean shell).
- Restoring on-screen content or scrollback (VT state is fresh/blank).
- Surviving a crash / `SIGKILL` / plain `kill` — only the explicit `asd restart`
  path restores workspaces. Any other daemon death loses sessions as today.
- Preserving the original spawn command. Sessions created with `--cmd` are
  recreated as a plain `$SHELL` (only the cwd is restored). [decision: option a]
- macOS cwd capture is best-effort (see Edge cases); Linux is the primary target.

## Design

No re-exec, no fd handoff, no change to the PTY/child backend. The existing
client-driven `stop → spawn` restart flow is kept; two things are added: the
daemon **saves a workspace-state file before it dies**, and a fresh daemon
**recreates sessions from that file on startup**.

### 1. Trigger — `asd restart` signals SIGUSR1

`asd-cli`'s `restart()` currently SIGTERMs the daemon (`stop_daemon`). It will
instead send **SIGUSR1**, which the daemon treats as "save workspaces, then shut
down." Everything else in the client flow is unchanged: wait for the socket to
disappear, spawn the new daemon, wait for it to accept connections.

- One-time caveat: the *first* restart from an old (pre-feature) daemon still
  loses sessions — an old daemon has SIGUSR1's default disposition (terminate)
  and won't save state. Every restart afterward restores workspaces. This is
  inherent and acceptable.
- A plain `kill`/SIGTERM/SIGINT continues to shut down **without** saving
  (sessions gone), unchanged from today.

### 2. Save workspace state (daemon, on SIGUSR1)

The daemon's `serve` loop selects on a new `SignalKind::user_defined1()` arm.
On SIGUSR1:

1. For each session in the registry, capture `{ name, cwd, cols, rows }`.
   - `cwd` = `readlink(/proc/<child_pid>/cwd)` (the session's direct child is its
     shell; its cwd is the workspace). `child_pid` is already tracked in
     `SessionMeta` (`child_pid`).
2. Serialize the list as JSON to a per-daemon **state file**
   `paths::restart_state_path(&socket)` (a sibling of `pid_path`, e.g.
   `<socket>.restart`).
3. Fall through to the **normal shutdown** path (SIGHUP children → 2s → SIGKILL,
   remove socket + pid file). The state file is intentionally left on disk for
   the successor.

### 3. Recreate on startup (new daemon)

In `run()`/`serve()`, right after binding the socket and before entering the
accept loop, if `paths::restart_state_path(&socket)` exists:

1. Read + parse it, then **delete it** (consume-once, so a later normal start
   doesn't resurrect stale sessions).
2. For each entry, `Registry::create` a session with the same `name`, the default
   shell (no `--cmd`), the saved `cols`/`rows`, and the saved `cwd`.
   - If the cwd no longer exists / isn't accessible, fall back to the process
     default (spawn without a cwd override) — never fail the whole startup for
     one bad dir.
3. Proceed to serve normally. Clients that reconnect see the sessions back, each
   a clean shell in its old directory.

### 4. Spawning with a cwd

`Registry::create` / `spawn_session` gain an optional `cwd: Option<PathBuf>`.
When set, `CommandBuilder::cwd(dir)` is called before the child is spawned
(`portable_pty` supports this directly). The `Frame::Create` path (interactive
`asd new`) passes `None`, preserving current behavior; only the restart-recreate
path passes a cwd. No protocol change — `cwd` is a daemon-internal argument.

## Components / changes

| Area | Change |
|---|---|
| `asd-proto::paths` | add `restart_state_path(socket) -> PathBuf` (sibling of `pid_path`) |
| `asd-daemon::session` | `spawn_session` + `Registry::create` take `cwd: Option<PathBuf>`; `CommandBuilder::cwd` when set |
| `asd-daemon` (`serve`) | SIGUSR1 arm → capture `{name,cwd,cols,rows}` per session, write state file, then shut down; on startup, consume the state file and recreate sessions |
| `asd-daemon` cwd read | `/proc/<pid>/cwd` readlink (Linux); macOS best-effort/none |
| `asd-cli::client` | `restart()` sends SIGUSR1 instead of SIGTERM |

Pure, unit-testable pieces: the state (de)serialization (`Vec<SessionState>` ↔
JSON round-trip) and `restart_state_path`. The cwd read and the recreate wiring
are covered by an e2e test.

## Error handling / edge cases

- **cwd unreadable or gone** → recreate without a cwd override (default dir),
  don't abort startup.
- **child already dead when SIGUSR1 arrives** (no `/proc/<pid>/cwd`) → skip cwd
  for that session (recreate in default dir) or skip the session entirely if it
  had already exited.
- **stale state file** (daemon wrote state then was SIGKILLed before the
  successor read it, and a later start finds it) → consume-once + delete makes a
  single spurious recreate at worst; acceptable. Optionally stamp the file and
  ignore if older than a few minutes (nice-to-have, not required).
- **macOS** → no `/proc`; cwd capture returns `None`, sessions recreate in the
  default dir. Full macOS cwd (libproc `PROC_PIDVNODEPATHINFO`) is a follow-up.
- **name collisions** on recreate → the registry is empty at that point (fresh
  daemon), so recreating saved names is conflict-free.

## Testing

- Unit: `SessionState` JSON round-trip; `restart_state_path` shape.
- e2e (`tests/e2e.rs`, real daemon): create a session, `cd` it into a known
  temp dir (via `asd send`), `asd restart`, then assert the session is back and
  its cwd is the temp dir (via `asd send 'pwd'` + `asd peek`, or by reading
  `/proc/<new child>/cwd`).

## Rollout note

Because the trigger is a signal (not a protocol frame), there is **no
`PROTO_VERSION` bump**. The one-time "first restart still loses sessions" caveat
above is the only migration wrinkle.
