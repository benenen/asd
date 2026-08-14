# Internal development documentation

The repository root [`README.md`](../README.md) is the user manual. This
directory contains implementation contracts and debugging boundaries for
contributors and coding agents. Read only the documents relevant to the area
being changed.

| Document | Read it when changing |
|---|---|
| [`architecture.md`](architecture.md) | Crate dependencies, daemon/session ownership, transports, lifecycle, persistence, paths, or configuration loading |
| [`terminal-behavior.md`](terminal-behavior.md) | Attach/TUI rendering, ownership, snapshots, modes, selection, paste, resize, or Windows Terminal behavior |
| [`automation.md`](automation.md) | `send`, `peek`, `wait`, `follow`, `card`, activity detection, or non-attached clients |
| [`cross-platform-development.md`](cross-platform-development.md) | Platform code, cross-compilation, filesystem semantics, CI, or release smoke checks |

Two nearby sources remain authoritative instead of being copied here:

- [`crates/asd-proto/src/lib.rs`](../crates/asd-proto/src/lib.rs) documents the
  current protocol version, wire history, frames, and error codes.
- [`crates/asd-dioxus/README.md`](../crates/asd-dioxus/README.md) documents the
  GUI actor, webview bridge, and JavaScript bundle.

When behavior changes, update the narrowest matching document. Keep
[`CLAUDE.md`](../CLAUDE.md) limited to rules that every task must load.
