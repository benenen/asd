# Design: `send` / `peek` / `wait` CLI commands

Port boo's three scripting commands to the asd CLI:

- `send <name> [flags]` — type into a session
- `peek <name>` — print the session's rendered screen
- `wait <name>` — block until output matches or settles

These make sessions scriptable (drive a build, wait for `PASS`, read the screen)
without an interactive attach. boo exposes them over a text control protocol;
asd uses a typed `Frame` protocol, so the port adds a few name-addressed,
attach-free frames.

## Goals

- Match boo's flag surface and semantics exactly (user decision).
- `send`/`peek` are one-shot and require **no attach** — usable while others
  are attached, or with nobody attached.
- `wait --idle` uses a real daemon-tracked pty-output-idle signal, like boo
  (user decision) — not client-side screen diffing.

## Non-goals

- No new interactive UI. These are batch/scripting commands.
- No multi-version protocol compatibility (asd never has it): the bump means
  daemon + clients rebuild together.

## 1. Protocol (`asd-proto`)

Bump `PROTO_VERSION` **3 → 4**. Add four frames:

| Frame | Direction | Purpose |
|---|---|---|
| `SendInput { name: String, bytes: Vec<u8> }` | client → daemon | write raw bytes to a named session's pty |
| `Ack` | daemon → client | generic success reply (answers `SendInput`) |
| `Peek { name: String, scrollback: bool }` | client → daemon | request a rendered screen dump |
| `PeekReply { cols: u16, rows: u16, cursor_col: u16, cursor_row: u16, title: String, screen: Vec<u8> }` | daemon → client | plain-text render + geometry |

`SessionInfo` gains one field: `idle_ms: u64` — milliseconds since the session's
last pty output (0 while actively producing / just created).

Lockstep updates (the test contract, spec §8):
- `crates/asd-proto/tests/codec.rs::all_frames()` — add the four frames.
- Both `SessionInfo` literals in that test — add `idle_ms`.
- The doc comment on `PROTO_VERSION` records the v4 additions.

`data_frame_size` is unchanged: `PeekReply` and `Ack` fall through its `_ => 0`
arm, so they ride the connection's outbound queue quota-free, exactly like
`History`. `PeekReply.screen` is bounded by the scrollback depth (~10k lines);
the 4 MiB frame cap remains the backstop.

## 2. Daemon (`asd-daemon`)

**`session.rs`**
- `SessionMeta` gains `last_output_ms: AtomicU64`, initialized to `created_ms`
  and stamped with "now" on each `SessionMsg::PtyOutput` batch.
- `SessionHandle::info()` computes `idle_ms = now_ms.saturating_sub(last_output_ms)`
  and puts it in `SessionInfo`.
- New `SessionMsg::Peek { sink: ClientSink, scrollback: bool }`. The session
  thread (the sole owner of the `!Send` VT) builds the reply from existing
  `VtBackend` methods — **no `asd-vt` change**:
  - cursor + geometry: `render_snapshot()` (`cursor.position`, `cols`, `rows`);
  - text rows: `fetch_history(start, count)` in screen space —
    visible screen = `fetch_history(scrollback_rows, rows)`,
    `--scrollback` = `fetch_history(0, history_len)`.
  - Join rows with `\n`; trim trailing blank lines (boo's peek behavior).

**`conn.rs`**
- `SendInput { name, bytes }`: `registry.get(name)` → send `SessionMsg::Input(bytes)`
  and `reply(Ack)`; missing → `Error { NO_SUCH_SESSION }`.
- `Peek { name, scrollback }`: `registry.get(name)` → make a fresh `ClientSink`
  on this connection's out-queue, send `SessionMsg::Peek { sink, scrollback }`;
  missing → `Error { NO_SUCH_SESSION }`.
- Neither reads or writes the connection's `attached` state.

## 3. CLI (`asd-cli`)

New module `crates/asd-cli/src/control.rs` holding the three async handlers plus
two pure helpers (`named_key`, `parse_duration`); `client_main` in `lib.rs`
dispatches to them. This keeps `lib.rs` focused on parsing/dispatch.

New `Cmd` variants (clap doc comments mirror boo's help text):

**`send <name>`** — `--text <t>` / `--key <list>` / `--enter` / `--stdin`
- `--text` and `--key` are mutually exclusive; `--stdin` excludes both; with
  none, read stdin (binary-safe).
- `--enter` appends `\r` after everything.
- Reject an empty payload and any NUL byte (NUL cannot cross the pty cleanly).
- Build the payload, send `SendInput`, expect `Ack` (else surface `Error`).
- `--key` names → bytes (comma-separated, same as boo):
  `Enter=\r`, `Tab=\t`, `Escape/Esc=\x1b`, `Space=" "`, `Backspace/BS=\x7f`,
  `Up=\x1b[A`, `Down=\x1b[B`, `Right=\x1b[C`, `Left=\x1b[D`, `Home=\x1b[H`,
  `End=\x1b[F`, `C-a`..`C-z` = control byte (`lower - 'a' + 1`).

**`peek <name>`** — `--scrollback` / `--json`
- Send `Peek { scrollback }`, receive `PeekReply`.
- Default: print `screen`, appending a newline if it does not end in one.
- `--json`: emit `{"session","title","rows","cols","cursor":{"row","col"},"screen"}`
  (screen JSON-escaped).

**`wait <name>`** — (`--text <t>` | `--idle`) `--timeout <dur>` (default `30s`)
- Exactly one of `--text` / `--idle` required.
- One persistent connection; poll every 50 ms:
  - `--text`: `Peek { scrollback:false }`, substring match on `screen` → exit 0.
  - `--idle`: `ListSessions`, find the session, `idle_ms >= 2000` → exit 0.
    Session gone → error exit.
  - Past the deadline: stderr message + **`std::process::exit(4)`** (boo's code).
- Durations: integer + unit `ms` / `s` / `m` / `h` (or `hr`) / `d`; `--flag=value`
  is handled natively by clap.

All three use `client::connect` (not `connect_or_spawn`): like `list`/`kill`
they require a running daemon and an existing session.

## 4. Error / exit codes

- `send` / `peek` failures (no such session, bad flags) → `anyhow` bail → exit 1.
- `wait` timeout → exit 4 (matches boo's documented code).
- `wait` on a vanished session → bail (exit 1) with a clear message.

## 5. Testing

- **`asd-proto`**: extend `all_frames()`; roundtrip the four new frames and the
  `idle_ms` field.
- **`asd-daemon` e2e** (`tests/`, real daemon): `send --text` reaches the pty
  (echoed back via a `peek`); `peek` renders the visible screen and, with
  `--scrollback`, the history; `idle_ms` rises after output goes quiet.
- **`asd-cli` unit**: `named_key()` maps every documented name (and rejects
  unknowns); `parse_duration()` parses each unit and rejects malformed input.

## Risks / notes

- Rebuilt CLI against an old daemon → `VERSION_MISMATCH`; the daemon must be
  restarted (`asd restart`). Same as any prior proto bump.
- A `peek --scrollback` on a maximally wide + deep buffer could approach the
  4 MiB frame cap; the natural ~10k-line scrollback bound keeps normal use far
  under it. Not special-cased.
