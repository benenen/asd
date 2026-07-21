# Persistent session list — design

**Date:** 2026-07-21
**Status:** approved, pending implementation
**Supersedes:** `2026-07-21-restart-preserve-workspace-design.md` (the SIGUSR1
consume-once `<socket>.restart` mechanism is replaced by the live list below).

## Goal

Make the daemon's set of sessions survive the daemon's own lifecycle. The
daemon maintains a persistent list of `{name, cwd}` and, on **every** startup,
recreates each listed session as a fresh shell `cd`'d to its saved directory.

Only the **name** and **working directory** are restored — not the live process,
its children, or the screen contents (identical scope to the previous restart
feature).

## Behavior

- **create** a session → append it to the list.
- **rename** a session → update its entry.
- **kill**, or the shell **exits on its own** → remove its entry.
- **daemon startup** (fresh boot, crash recovery, or `asd restart`) → recreate
  every listed session.

A session that was killed/exited before shutdown is already off the list, so it
is not resurrected.

## Storage

- **Path:** `paths::session_list_path()` = `<XDG_DATA_HOME|~/.local/share>/asd/sessions.tsv`.
  Lives in the data directory (persistent across reboots), separate from the
  read-only user `config.toml`.
- **Format:** the existing TSV — one `name\tcwd` line per session (cwd empty when
  unknown). Reuses `SessionState`, `serialize`, `parse`, `read_cwd`.
- **Atomic writes:** write to a sibling temp file then `rename` over the target,
  so a crash mid-write cannot leave a torn file.
- **Scope:** one file per data directory. A second daemon on a custom
  `$ASD_SOCKET` but the same `$HOME`/`$XDG_DATA_HOME` would share it — a
  documented limitation, acceptable for the normal single-daemon case. (Tests
  isolate via `XDG_DATA_HOME`, so this does not affect them.)

## Architecture

Rename `crates/asd-daemon/src/restart.rs` → `store.rs`, reflecting its broader
role (a live session store rather than a one-shot restart snapshot).

`store.rs`:
- `SessionState { name: String, cwd: Option<PathBuf> }` — unchanged.
- `read_cwd(pid) -> Option<PathBuf>` — unchanged (`/proc/<pid>/cwd`).
- `serialize(&[SessionState]) -> String` / `parse(&str) -> Vec<SessionState>` — unchanged.
- `write_atomic(path, &[SessionState])` — new; temp-file + rename. Best effort:
  a write failure only logs a warning (persistence degrades, daemon keeps
  serving).
- `read(path) -> Vec<SessionState>` — replaces `take()`; reads + parses **without
  deleting** (absent file → empty). The list is the live source of truth, not
  consume-once.
- Drop `take()`.

`registry.rs` — `Registry` gains:
- `persist_path: PathBuf` and `persist_frozen: bool` fields.
- Constructor becomes `Registry::new(scrollback_lines, persist_path)`.
- `snapshot() -> Vec<SessionState>` — the current `capture_restart_state`,
  renamed; reads each session's live name (`meta.name`) + `read_cwd(pid)`.
- `persist(&self)` — if not frozen, `store::write_atomic(&self.persist_path,
  &self.snapshot())`. Called at the end of `create`, `rename`, and `remove`.
- `freeze_and_persist(&mut self)` — one final `store::write_atomic` (capturing
  live cwds), then sets `persist_frozen = true`.

`lib.rs::serve`:
- `let persist_path = paths::session_list_path();`
- `Registry::new(config.scrollback_lines, persist_path.clone())`.
- **Startup restore:** `for st in store::read(&persist_path) { Registry::create(
  &registry, Some(st.name), None, st.cwd) }` (each create re-persists the file).
- **Signals:** SIGTERM, SIGINT, and SIGUSR1 are **unified** — each does
  `registry.lock().freeze_and_persist()` then breaks out of the accept loop.
  Then `shutdown_all` SIGHUPs the children; their EOF-driven `registry.remove`
  calls are no-ops on the file because `persist_frozen` is set.
- Remove the old `restart::take` startup block and the SIGUSR1-specific capture.

`paths.rs`:
- Add `session_list_path() -> PathBuf` (= `data_dir().join("sessions.tsv")`).
- Remove `restart_state_path(socket)`.

`asd-cli/src/client.rs`:
- `stop_daemon` keeps sending SIGUSR1 for `asd restart` — unchanged; it now flows
  through the unified persist+shutdown path.

## The shutdown-freeze invariant

The critical correctness point: during shutdown the daemon SIGHUPs every child,
each child EOFs, and each session thread calls `registry.remove`. Without a
guard, those removals would rewrite the file to *empty* immediately before the
restart that is supposed to restore them.

Resolution: on any terminating signal the daemon calls `freeze_and_persist`
first — it writes the final list (with fresh cwds) and sets `persist_frozen`.
`remove` (and `persist` generally) does nothing while frozen. Shutdown-induced
removals therefore leave the file intact.

- Clean stop / `asd restart` → final persist captures live cwds → accurate restore.
- Hard crash (SIGKILL) → no freeze runs; the file holds the last write from the
  most recent create/rename/remove → restore with best-effort (possibly slightly
  stale) cwd.

## cwd freshness

cwd is read live from `/proc/<pid>/cwd` at each rewrite and at the final shutdown
persist, so a clean stop/restart restores *where the user actually `cd`'d to*.
Non-Linux or an unreadable link → `None` → the session recreates in `$HOME`
(existing `spawn_session` fallback).

## Error handling

- Missing state file → empty list → nothing to restore (normal first run).
- Malformed/blank lines → skipped by `parse` (existing behavior).
- Write failure → warning only; the daemon keeps serving, persistence degrades.
- A restore-create that fails (e.g. invalid saved name) → logged and skipped; the
  next `persist` drops it from the file.

## Testing

`store.rs` unit tests: keep `parse`/`serialize` round-trip and `read_cwd(0)`
tests; add a `write_atomic` → `read` round-trip test.

e2e (`tests/e2e.rs`):
- **restore across a full stop:** create sessions, `cd` one, SIGTERM the daemon,
  start a fresh daemon on the same socket + `XDG_DATA_HOME`, assert the sessions
  come back (by `ListSessions`) with the expected cwd (via `Inspect`/`peek`).
- **killed sessions are not resurrected:** create two, kill one, restart, assert
  only the survivor returns.
- **rename persists:** create, rename, restart, assert it returns under the new
  name.
- Update the two existing restart tests (`restart_preserves_session_workspace`,
  `restart_replaces_the_daemon`) to the new `session_list_path()` and semantics.

## Out of scope

- Persisting live process state / screen contents (unchanged: only name + cwd).
- Per-socket state files / multi-daemon-per-user isolation (single file per data
  dir).
- Capturing cwd drift between mutations without a shutdown (best-effort only).
