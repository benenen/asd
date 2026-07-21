# Persistent Session List Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist the daemon's session set (`{name, cwd}`) to a data-dir file that is updated on create/rename/kill and restored on every daemon startup.

**Architecture:** The `Registry` owns a persist path and rewrites the whole list (atomic temp+rename) on every mutation. On startup `serve()` reads the list and recreates each session as a fresh shell `cd`'d to its saved cwd. On shutdown the daemon does one final persist then freezes persistence, so the SIGHUP-driven session removals don't wipe the file. This subsumes the old SIGUSR1 `<socket>.restart` snapshot; that module is renamed `restart.rs` → `store.rs`.

**Tech Stack:** Rust, tokio (daemon net side), std threads (session), the existing TSV serialization in `restart.rs`.

## Global Constraints

- All code comments in English (doc comments, inline, Cargo.toml).
- `cargo fmt --all` clean; `cargo clippy --workspace --all-targets` clean.
- **No `PROTO_VERSION` bump** — this feature touches no wire frames.
- Only **name + cwd** are restored — never the live process, children, or screen.
- cwd is read live from `/proc/<pid>/cwd`; unreadable/non-Linux → `None` → session recreates in `$HOME` (existing `spawn_session` fallback).
- Persistence is best-effort: a write failure only logs a warning; the daemon keeps serving.
- Builds/tests may use `--offline` (deps are cached). Push to `main` via the benenen token: `git push "https://benenen:$(gh auth token -u benenen)@github.com/benenen/asd.git" main`.

## File Structure

- `crates/asd-proto/src/paths.rs` — **modify**: add `session_list_path()`, remove `restart_state_path()`.
- `crates/asd-daemon/src/restart.rs` → `crates/asd-daemon/src/store.rs` — **rename + modify**: add `write_atomic` + `read`, drop `take`/`write`, update the module doc.
- `crates/asd-daemon/src/registry.rs` — **modify**: `Registry` gains `persist_path`/`persist_frozen`; `new(scrollback_lines, persist_path)`; `snapshot()` (renamed from `capture_restart_state`), `persist()`, `freeze_and_persist()`; hook `persist()` into `create`/`rename`/`remove`.
- `crates/asd-daemon/src/lib.rs` — **modify**: `mod restart;` → `mod store;`; construct `Registry::new(.., session_list_path())`; restore via `store::read`; unify SIGTERM/SIGINT/SIGUSR1 and `freeze_and_persist()` on the way out.
- `tests/e2e.rs` — **modify**: repoint `restart_preserves_session_workspace` at the new file; add a `respawn_successor` harness helper + 3 new tests.

---

### Task 1: `paths::session_list_path()`

**Files:**
- Modify: `crates/asd-proto/src/paths.rs`
- Test: `crates/asd-proto/src/paths.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Produces: `pub fn session_list_path() -> std::path::PathBuf` = `data_dir().join("sessions.tsv")`.
- Removes: `pub fn restart_state_path(socket: &Path) -> PathBuf` (Task 2 drops its last callers).

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/asd-proto/src/paths.rs`:

```rust
    #[test]
    fn session_list_path_is_sessions_tsv_in_data_dir() {
        let p = session_list_path();
        assert_eq!(p.file_name().unwrap(), std::ffi::OsStr::new("sessions.tsv"));
        assert_eq!(p.parent().unwrap(), data_dir());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p asd-proto session_list_path --offline`
Expected: FAIL — `cannot find function 'session_list_path' in this scope`.

- [ ] **Step 3: Add `session_list_path`, remove `restart_state_path`**

In `crates/asd-proto/src/paths.rs`, delete this function:

```rust
/// Where the daemon records each session's workspace (cwd) when asked to restart
/// (SIGUSR1); the successor daemon consumes it to recreate the sessions in their
/// old directories. Per-socket, a sibling of the pid file.
pub fn restart_state_path(socket: &Path) -> PathBuf {
    socket.with_extension("restart")
}
```

and add, immediately after `data_dir()`:

```rust
/// Path of the persisted session list: `<data_dir>/sessions.tsv`. The daemon
/// rewrites it on every session create/rename/kill and restores from it on every
/// startup. Lives in the (persistent) data directory, keyed by it — a single
/// daemon per data directory. Read-write daemon state, distinct from the
/// read-only user `config.toml`.
pub fn session_list_path() -> PathBuf {
    data_dir().join("sessions.tsv")
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p asd-proto --offline`
Expected: PASS (all `asd-proto` tests, including the new one).

Note: `crates/asd-daemon` will not compile until Task 2 (it still references `restart_state_path`). That is expected; do not build the whole workspace yet.

- [ ] **Step 5: Commit**

```bash
git add crates/asd-proto/src/paths.rs
git commit -m "asd-proto: add paths::session_list_path (drop restart_state_path)"
```

---

### Task 2: Live-persist mechanism (store + Registry + serve)

Swap the SIGUSR1 consume-once `<socket>.restart` snapshot for a continuously-maintained `sessions.tsv` restored on every startup. This is one cohesive change: the store layer, the registry hooks, and the serve wiring all move together, and the existing `restart_preserves_session_workspace` e2e is repointed to the new file so the tree stays green.

**Files:**
- Rename: `crates/asd-daemon/src/restart.rs` → `crates/asd-daemon/src/store.rs`
- Modify: `crates/asd-daemon/src/registry.rs`
- Modify: `crates/asd-daemon/src/lib.rs`
- Modify: `tests/e2e.rs` (repoint one existing test)

**Interfaces:**
- Consumes: `asd_proto::paths::session_list_path()` (Task 1).
- Produces:
  - `store::write_atomic(path: &Path, states: &[SessionState])`
  - `store::read(path: &Path) -> Vec<SessionState>`
  - `Registry::new(scrollback_lines: usize, persist_path: PathBuf) -> Registry`
  - `Registry::snapshot(&self) -> Vec<store::SessionState>`
  - `Registry::freeze_and_persist(&mut self)`
  - (private) `Registry::persist(&self)`

- [ ] **Step 1: Rename the module file**

```bash
git mv crates/asd-daemon/src/restart.rs crates/asd-daemon/src/store.rs
```

- [ ] **Step 2: Update the module doc, replace `write`/`take` with `write_atomic`/`read`**

In `crates/asd-daemon/src/store.rs`, replace the top doc comment:

```rust
//! Persistent session list. The daemon keeps `{name, cwd}` for every live
//! session in a data-dir file (`paths::session_list_path`), rewritten on every
//! create/rename/kill and restored on every startup — each session comes back as
//! a fresh shell `cd`'d to its saved directory. Only the cwd is restored, not the
//! live process or the screen.
//!
//! Spec: docs/superpowers/specs/2026-07-21-persistent-session-list-design.md
```

Then delete the old `write` and `take` functions:

```rust
/// Write the state file (best effort; a failure just means workspaces aren't
/// restored on the next start).
pub fn write(path: &Path, states: &[SessionState]) {
    if let Err(e) = std::fs::write(path, serialize(states)) {
        tracing::warn!(path = %path.display(), error = %e, "failed to write restart state");
    }
}

/// Read and consume (delete) the state file, returning the sessions to
/// recreate. An absent file yields an empty list. Consume-once so a later normal
/// start doesn't resurrect stale sessions.
pub fn take(path: &Path) -> Vec<SessionState> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let _ = std::fs::remove_file(path);
    parse(&text)
}
```

and add in their place:

```rust
/// Atomically write the session list: write a sibling temp file, then `rename`
/// it over `path` (atomic on the same filesystem), so a crash mid-write cannot
/// leave a torn file. Best effort — a failure only logs a warning.
pub fn write_atomic(path: &Path, states: &[SessionState]) {
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, serialize(states)) {
        tracing::warn!(path = %tmp.display(), error = %e, "failed to write session list");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        tracing::warn!(path = %path.display(), error = %e, "failed to install session list");
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Read and parse the session list, WITHOUT deleting it — the file is the live
/// source of truth, not consume-once. An absent/unreadable file yields an empty
/// list.
pub fn read(path: &Path) -> Vec<SessionState> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(_) => Vec::new(),
    }
}
```

- [ ] **Step 3: Add a `write_atomic` → `read` round-trip test**

In `crates/asd-daemon/src/store.rs`, add to the `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn write_atomic_then_read_round_trips() {
        let dir = std::env::temp_dir().join(format!("asd-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.tsv");
        let states = vec![
            SessionState { name: "web".into(), cwd: Some(PathBuf::from("/home/me/proj")) },
            SessionState { name: "s0".into(), cwd: None },
        ];
        write_atomic(&path, &states);
        assert_eq!(read(&path), states);
        // An absent file reads as an empty list.
        std::fs::remove_file(&path).unwrap();
        assert!(read(&path).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 4: `mod` rename in lib.rs**

In `crates/asd-daemon/src/lib.rs`, change `mod restart;` to `mod store;` (keep the four modules alphabetical: `config`, `conn`, `registry`, `session`, `store`).

- [ ] **Step 5: Registry — fields, constructor, snapshot/persist/freeze**

In `crates/asd-daemon/src/registry.rs`, add the import at the top (with the other `use`s):

```rust
use std::path::PathBuf;
```

Replace the struct + `new` with:

```rust
pub struct Registry {
    sessions: HashMap<String, SessionHandle>,
    /// Auto-naming counter for `s0`, `s1`, ... — monotonically increasing
    /// (avoids reusing a name that just died).
    next_auto: u64,
    /// Scrollback depth (lines) applied to every session this registry spawns;
    /// comes from the daemon config, resolved once at startup.
    scrollback_lines: usize,
    /// Where the live session list is persisted; rewritten on every mutation.
    persist_path: PathBuf,
    /// Once set (at shutdown), `persist` is a no-op — so the SIGHUP-driven
    /// session removals during shutdown don't wipe the file before restart.
    persist_frozen: bool,
}

impl Registry {
    /// Create an empty registry whose sessions each keep `scrollback_lines` lines
    /// of scrollback and whose live set is persisted to `persist_path`.
    pub fn new(scrollback_lines: usize, persist_path: PathBuf) -> Self {
        Self {
            sessions: HashMap::new(),
            next_auto: 0,
            scrollback_lines,
            persist_path,
            persist_frozen: false,
        }
    }
```

Rename `capture_restart_state` to `snapshot` (keep its body; only the name and doc change):

```rust
    /// Snapshot each live session's name + cwd for persistence/restore. Reads
    /// `/proc/<pid>/cwd` under the lock — a cheap readlink.
    pub fn snapshot(&self) -> Vec<crate::store::SessionState> {
        self.sessions
            .values()
            .map(|h| {
                let name = h
                    .meta
                    .name
                    .lock()
                    .map(|n| n.clone())
                    .unwrap_or_else(|_| h.name.clone());
                let pid = h.meta.child_pid.load(std::sync::atomic::Ordering::Relaxed);
                crate::store::SessionState {
                    name,
                    cwd: crate::store::read_cwd(pid),
                }
            })
            .collect()
    }

    /// Rewrite the persisted session list from the live set (no-op while frozen).
    fn persist(&self) {
        if self.persist_frozen {
            return;
        }
        crate::store::write_atomic(&self.persist_path, &self.snapshot());
    }

    /// Final persist (capturing live cwds), then freeze so the shutdown SIGHUPs'
    /// session removals don't clobber the file. Called once on the way out.
    pub fn freeze_and_persist(&mut self) {
        crate::store::write_atomic(&self.persist_path, &self.snapshot());
        self.persist_frozen = true;
    }
```

- [ ] **Step 6: Hook `persist()` into create/rename/remove**

In `crates/asd-daemon/src/registry.rs`:

In `create`, change the tail (after the successful spawn):

```rust
        reg.sessions.insert(name.clone(), handle);
        reg.persist();
        info!(session = %name, "session created");
        Ok(name)
```

In `rename`, add the persist just before `Ok(())`:

```rust
        self.sessions.insert(new.to_string(), handle);
        self.persist();
        info!(from = %old, to = %new, "session renamed");
        Ok(())
```

Replace `remove`:

```rust
    /// Callback at the session thread's endpoint: deregister and re-persist (so a
    /// killed or self-exited session drops off the list). A no-op on the file
    /// during shutdown, where `persist_frozen` is set.
    pub fn remove(&mut self, name: &str) {
        self.sessions.remove(name);
        self.persist();
    }
```

- [ ] **Step 7: serve() — construct with persist path, restore, unified shutdown**

In `crates/asd-daemon/src/lib.rs`, in `serve()`, replace the registry construction and the old restart-restore block:

```rust
    // Load config (scrollback depth, …) once at startup; a missing/broken file
    // falls back to defaults. It governs every session this daemon spawns —
    // change it and `asd restart` to apply.
    let config = config::Config::load(&paths::config_path());
    let persist_path = paths::session_list_path();
    let registry = Arc::new(Mutex::new(Registry::new(
        config.scrollback_lines,
        persist_path.clone(),
    )));

    // Restore the persisted session list on every startup (fresh boot, crash
    // recovery, or `asd restart`): recreate each saved session as a fresh shell
    // `cd`'d to its saved cwd. Each create re-persists the file.
    for st in store::read(&persist_path) {
        match Registry::create(&registry, Some(st.name.clone()), None, st.cwd) {
            Ok(_) => info!(session = %st.name, "session restored"),
            Err((code, msg)) => warn!(session = %st.name, code, %msg, "restore failed"),
        }
    }
```

Replace the `tokio::select!` signal arms so all three just break:

```rust
            _ = sigterm.recv() => { info!("SIGTERM received"); break; }
            _ = sigint.recv() => { info!("SIGINT received"); break; }
            _ = sigusr1.recv() => { info!("SIGUSR1 received"); break; }
```

Immediately after the `loop { ... }` (before `shutdown_all`), replace the old
"Sessions are not persisted" comment with the final persist + freeze:

```rust
    // Capture final cwds and freeze the session list before killing children:
    // the SIGHUP-driven `Registry::remove` calls below must not erase the file,
    // so the next daemon can restore from it.
    registry.lock().unwrap().freeze_and_persist();

    // Shutdown contract (spec §5): SIGHUP each child → wait 2s → SIGKILL
    // stragglers → remove socket.
    let reg = Arc::clone(&registry);
    tokio::task::spawn_blocking(move || Registry::shutdown_all(&reg)).await?;
```

(Leave the `sigusr1` signal *registration* line as-is; only its arm body changed. Delete the now-unused `let state_path = ...` line if one remains.)

- [ ] **Step 8: Repoint the existing restart-workspace e2e at the new file**

In `tests/e2e.rs`, in `restart_preserves_session_workspace`, replace the state-file read:

```rust
    // The persisted session list records name + cwd (in the daemon's data dir).
    let state = std::fs::read_to_string(daemon.dir.join("data/asd/sessions.tsv"))
        .expect("session list written");
```

(Everything else in that test — SIGUSR1, successor daemon, `list`/`pwd` assertions — is unchanged. `restart_replaces_the_daemon` needs no change: it never referenced the state-file path.)

- [ ] **Step 9: Build, fmt, clippy**

Run:
```bash
cargo fmt --all
cargo clippy --workspace --all-targets --offline
```
Expected: no warnings, no errors. (No dead code: `store::write_atomic` is used by `persist`/`freeze_and_persist`; `store::read` by `serve`.)

- [ ] **Step 10: Run daemon unit + existing e2e**

Run:
```bash
cargo test -p asd-daemon --offline
cargo test --test e2e --offline
```
Expected: all pass — including the store round-trip test, `restart_preserves_session_workspace` (now reading `sessions.tsv`), and `restart_replaces_the_daemon`.

- [ ] **Step 11: Commit**

```bash
git add crates/asd-daemon/src/store.rs crates/asd-daemon/src/registry.rs crates/asd-daemon/src/lib.rs tests/e2e.rs
git commit -m "daemon: persist the live session list; restore on every startup

Rename restart.rs -> store.rs; the Registry now rewrites <data-dir>/sessions.tsv
on every create/rename/kill and serve() restores it on every startup. A final
persist + freeze on the way out keeps shutdown's SIGHUP removals from wiping the
file. Replaces the SIGUSR1 consume-once <socket>.restart snapshot."
```

---

### Task 3: New e2e coverage

Prove the three behaviors the design promises: restore across a plain stop, no resurrection of killed sessions, and rename persistence.

**Files:**
- Modify: `tests/e2e.rs`

**Interfaces:**
- Consumes: `Daemon` harness, `ProtoClient`, `Frame::Rename { name, new_name }` → `Frame::Ack`, the `WAIT`/`TICK` constants, `cli_exe()`.
- Produces (test-only): `Daemon::respawn_successor(&self) -> std::process::Child`.

- [ ] **Step 1: Add a successor-daemon harness helper**

In `tests/e2e.rs`, add a method to the `impl Daemon` block:

```rust
    /// Start a fresh daemon on the same socket + data dir (as a detached process,
    /// not our child) and wait for it to accept connections. Used to test restore
    /// after the original daemon has stopped. Returns the child so the caller can
    /// SIGTERM it at the end.
    fn respawn_successor(&self) -> std::process::Child {
        let child = Command::new(cli_exe())
            .arg("daemon")
            .arg("--socket")
            .arg(&self.socket)
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + WAIT;
        while !self.socket.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "successor daemon never came up"
            );
            std::thread::sleep(TICK);
        }
        child
    }

    /// SIGTERM this daemon and wait for its socket to disappear.
    fn stop_and_wait(&self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + WAIT;
        while self.socket.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "daemon didn't shut down after SIGTERM"
            );
            std::thread::sleep(TICK);
        }
    }
```

- [ ] **Step 2: Test — sessions persist across a plain SIGTERM stop**

Add to `tests/e2e.rs`:

```rust
/// A plain daemon stop (SIGTERM, not `asd restart`) still persists the session
/// list, and a fresh daemon restores every session — cwd included.
#[tokio::test]
async fn sessions_persist_across_a_full_stop() {
    let daemon = Daemon::start("persist");
    for name in ["web", "logs"] {
        assert!(daemon.cli().args(["new", name]).status().unwrap().success());
    }
    let workdir = daemon.dir.join("web-workspace");
    std::fs::create_dir_all(&workdir).unwrap();
    daemon
        .cli()
        .args([
            "send",
            "web",
            "--text",
            &format!("cd '{}' && echo READY", workdir.display()),
            "--enter",
        ])
        .status()
        .unwrap();
    assert!(
        daemon
            .cli()
            .args(["wait", "web", "--text", "READY", "--timeout", "10s"])
            .status()
            .unwrap()
            .success(),
        "web never reached READY"
    );

    daemon.stop_and_wait();
    let mut successor = daemon.respawn_successor();

    let list = daemon.cli().args(["list"]).output().unwrap();
    let list = String::from_utf8_lossy(&list.stdout);
    assert!(list.contains("web") && list.contains("logs"), "both restored: {list}");

    daemon.cli().args(["send", "web", "--text", "pwd", "--enter"]).status().unwrap();
    assert!(
        daemon
            .cli()
            .args(["wait", "web", "--text", workdir.to_str().unwrap(), "--timeout", "10s"])
            .status()
            .unwrap()
            .success(),
        "web not restored in its saved cwd"
    );

    unsafe { libc::kill(successor.id() as i32, libc::SIGTERM) };
    let _ = successor.wait();
}
```

- [ ] **Step 3: Test — a killed session is not resurrected**

Add to `tests/e2e.rs`:

```rust
/// Killing a session removes it from the persisted list, so a restart does not
/// bring it back — only the survivors return.
#[tokio::test]
async fn killed_session_is_not_restored() {
    let daemon = Daemon::start("nokill");
    for name in ["keep", "doomed"] {
        assert!(daemon.cli().args(["new", name]).status().unwrap().success());
    }
    assert!(daemon.cli().args(["kill", "doomed"]).status().unwrap().success());

    daemon.stop_and_wait();
    let mut successor = daemon.respawn_successor();

    let list = daemon.cli().args(["list"]).output().unwrap();
    let list = String::from_utf8_lossy(&list.stdout);
    assert!(list.contains("keep"), "survivor missing: {list}");
    assert!(!list.contains("doomed"), "killed session resurrected: {list}");

    unsafe { libc::kill(successor.id() as i32, libc::SIGTERM) };
    let _ = successor.wait();
}
```

- [ ] **Step 4: Test — a rename persists across restart**

Add to `tests/e2e.rs` (rename is protocol-only, so drive it via `ProtoClient`):

```rust
/// Renaming a session updates the persisted list, so a restart restores it under
/// the new name (and not the old).
#[tokio::test]
async fn rename_persists_across_restart() {
    let daemon = Daemon::start("rename");
    assert!(daemon.cli().args(["new", "before"]).status().unwrap().success());

    let mut c = ProtoClient::connect(&daemon.socket).await;
    c.send(Frame::Rename {
        name: "before".into(),
        new_name: "after".into(),
    })
    .await;
    assert!(matches!(c.recv().await, Frame::Ack), "rename not acked");
    drop(c);

    daemon.stop_and_wait();
    let mut successor = daemon.respawn_successor();

    let list = daemon.cli().args(["list"]).output().unwrap();
    let list = String::from_utf8_lossy(&list.stdout);
    assert!(list.contains("after"), "renamed session missing: {list}");
    assert!(!list.contains("before"), "old name still present: {list}");

    unsafe { libc::kill(successor.id() as i32, libc::SIGTERM) };
    let _ = successor.wait();
}
```

- [ ] **Step 5: Run the new tests**

Run:
```bash
cargo test --test e2e --offline sessions_persist_across_a_full_stop killed_session_is_not_restored rename_persists_across_restart
```
Expected: 3 passed.

- [ ] **Step 6: Full e2e + clippy sweep**

Run:
```bash
cargo test --test e2e --offline
cargo clippy --workspace --all-targets --offline
cargo fmt --all -- --check
```
Expected: all e2e pass; clippy clean; fmt clean.

- [ ] **Step 7: Commit**

```bash
git add tests/e2e.rs
git commit -m "e2e: cover session-list persistence (stop/restore, no-resurrect, rename)"
```

---

### Task 4: Docs + final verification

**Files:**
- Modify: `CLAUDE.md` (session-lifecycle line)

**Interfaces:** none.

- [ ] **Step 1: Update the session-lifecycle note in CLAUDE.md**

In `CLAUDE.md`, replace the sentence that begins `session 的活进程/屏幕不持久化` (added by the restart-workspace feature) with:

```
session 的活进程/屏幕不持久化——但 name+cwd 会：`Registry` 每次 create/rename/kill 把全部 session 的 `{name, cwd}` 原子写到 `paths::session_list_path()`（`<data_dir>/sessions.tsv`，`store::write_atomic` 临时文件+rename），daemon **每次启动**都 `store::read` 复原（同名默认 shell + 原 cwd 的新 shell，含崩溃恢复/`asd restart`；`registry.snapshot` 出、cwd 读 `/proc/<pid>/cwd` 取不到回落 `$HOME`）。关键不变量：关机时先 `freeze_and_persist`（读 live cwd 写一次 + 置 `persist_frozen`），使随后 SIGHUP 收尸触发的 `registry.remove` 不清空文件。kill/shell 自己退出=从列表移除、不复活；硬 SIGKILL 保留最后一次写入的 cwd。取代了旧的 SIGUSR1 consume-once `<socket>.restart`（模块 `restart.rs`→`store.rs`）。见 `crates/asd-daemon/src/store.rs`。
```

- [ ] **Step 2: Full workspace verification**

Run:
```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --offline
cargo build --workspace --offline
cargo test --workspace --offline
```
Expected: fmt clean; clippy clean; build ok; all tests pass.

- [ ] **Step 3: Commit + push**

```bash
git add CLAUDE.md
git commit -m "docs(CLAUDE): session list now persists across every daemon restart"
git push "https://benenen:$(gh auth token -u benenen)@github.com/benenen/asd.git" main
```

---

## Self-Review

**Spec coverage:**
- Storage (data-dir `sessions.tsv`, TSV, atomic write) → Task 1 (path) + Task 2 Steps 2–3.
- create/rename/kill hooks → Task 2 Step 6.
- Restore on every startup → Task 2 Step 7.
- Shutdown-freeze invariant → Task 2 Steps 5, 7 (`freeze_and_persist` + `persist_frozen`).
- cwd freshness (live `/proc/<pid>/cwd`) → Task 2 Step 5 (`snapshot`).
- Replaces restart.rs/SIGUSR1 → Task 2 Steps 1–2, 7; paths cleanup Task 1.
- Tests: store unit → Task 2 Step 3; 3 new e2e → Task 3; existing restart tests → Task 2 Step 8 (+ `restart_replaces_the_daemon` needs no change).
- Docs → Task 4.

**Placeholder scan:** none — every code step shows full code; every command has expected output.

**Type consistency:** `write_atomic(&Path, &[SessionState])`, `read(&Path) -> Vec<SessionState>`, `Registry::new(usize, PathBuf)`, `snapshot(&self) -> Vec<store::SessionState>`, `persist(&self)`, `freeze_and_persist(&mut self)`, `Frame::Rename { name, new_name }` → `Frame::Ack` — used consistently across tasks. `session_list_path()` (Task 1) matches the daemon's data-dir path `dir/data/asd/sessions.tsv` asserted in the e2e tests (daemon runs with `XDG_DATA_HOME=dir/data`).
