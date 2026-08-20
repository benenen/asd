# CLAUDE.md

This file is the always-loaded agent guide for this repository. `AGENTS.md` is
a symlink to it. Keep this file short: durable implementation details belong in
[`docs/`](docs/README.md), while public installation and usage belong in
[`README.md`](README.md).

## Project boundaries

`asd` is a GPU terminal client plus a headless session daemon. It is closer to
shpool than tmux: one session owns exactly one PTY; there are no panes or
windows inside a session.

The repository intentionally ships one root-package `asd` executable. The
CLI/TUI/daemon/PTY side is always built in; the only feature is `dioxus`
(`gui` is an alias), which adds the desktop GUI and is on by default. Bare
`asd` opens the GUI. A headless server uses `--no-default-features`.

## Non-negotiable rules

- Write all code comments in English, including doc comments, inline comments,
  and comments in Cargo files.
- Any new protocol frame or frame-shape change must bump
  `asd_proto::PROTO_VERSION` and update `all_frames()` in
  `crates/asd-proto/tests/codec.rs`, including its explicit version assertion.
  Both endpoints upgrade together; there is no multi-version compatibility.
  Update the protocol module history and run `cargo test -p asd-proto`.
- Put Unix/Windows differences behind each crate's `src/platform/` module.
  Call sites must not introduce `#[cfg(unix)]` or `#[cfg(windows)]`. Both
  platform implementations must export the same explicit interface through
  `platform/mod.rs`. The Linux/macOS/Windows foreground-process lookup is the
  existing exception because it is a three-way OS split.
- Preserve crate dependency boundaries. In particular, `asd-client` and
  `asd-dioxus` must remain free of PTY/process-management dependencies, and GUI
  frameworks must not leak into CLI or daemon crates.
- Update the matching document under `docs/` when an implementation change
  alters one of its stated invariants. Do not grow this file with incident
  histories or command tutorials.

The complete crate ownership table is in
[`docs/architecture.md`](docs/architecture.md#crate-boundaries).

## Runtime invariants

- Each session has a blocking PTY reader thread and one session thread. The
  session thread exclusively owns its `GhosttyVt`; PTY output and all session
  messages are serialized through its channel. This ordering is what makes an
  attach Snapshot precede later Output.
- The daemon is the only owner of the session-side terminal model and query
  responses. Rendering clients may keep a local VT, but they must drain/drop
  its generated PTY replies rather than answering the application themselves.
- Ordinary `asd attach` clients and the desktop GUI are shared clients. Only
  `asd ui` has one exclusive interactive viewer per session; a new TUI viewer
  displaces the previous TUI without displacing shared clients.
- PTY size is the per-axis minimum of all still-attached clients. A revoked or
  dead client must be removed from both membership and size negotiation.
- Follow subscriptions are not attachments: they receive Output and activity
  status but do not receive Snapshots, count as attached clients, or affect PTY
  size.
- `asd daemon` owns a Tokio runtime. Dispatch that subcommand before entering
  any outer `#[tokio::main]` runtime; nested runtimes are invalid.
- libghostty-vt is an unstable dependency. Direct upstream API calls stay in
  `crates/asd-vt/src/ghostty.rs`; other crates use `VtBackend` and
  `RenderSnapshot`.

## Required checks

Run checks proportional to the change. Before handing off a repository-wide
change, use:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
```

`cargo test` without `--workspace` only tests the root package. Protocol work
also requires `cargo test -p asd-proto`. Platform-sensitive work must follow
[`docs/cross-platform-development.md`](docs/cross-platform-development.md),
including the Windows GNU cross-check when applicable.
