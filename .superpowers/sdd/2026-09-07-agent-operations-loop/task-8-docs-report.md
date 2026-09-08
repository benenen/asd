# Task 8 documentation report

## Scope

Updated only the assigned public-contract documentation:

- `README.md`
- `docs/automation.md`
- `docs/architecture.md`
- `docs/cross-platform-development.md`
- `config.example.toml`
- `crates/asd-dioxus/README.md`

## Reconciled contracts

- `wait --regex` is Rust regex over the visible rendered screen only. Literal
  and regex waits atomically check current terminal state, then match each
  post-feed screen; idle/state waits use the event stream.
- Event reconnects either replay a bounded contiguous suffix or replace the
  projection on reset. Snapshot activity uses one age sample for `idle_ms` and
  `running`; lifecycle and detection facts remain cursor ordered.
- Detector explain uses the live daemon terminal, while reload applies a new
  generation to quiet sessions, reports diagnostics, and returns partial
  completion after five seconds without rolling back the active generation.
- Done/Seen is local client presentation state. Exact snapshot convergence,
  plus GUI focus, is required for Seen; independent GUI and TUI notification
  leases deduplicate external alerts.
- Codex and Claude synchronous hook examples preserve vendor JSON on stdin and
  document exact source/reason allowlists. `ASD_SESSION_ID` finds the hosting
  session but cannot prove foreground-agent identity, so unsupported foreground
  proof can reject manually launched Windows agents.
- `sessions.json` is versioned and authoritative. TSV migration retains the
  `.tsv.migrated` backup after JSON commit/read-back; corrupt JSON never falls
  back. Normal restore stages commands without Enter, protects control
  characters, and deterministically prevents duplicate resume claims from
  starting a fresh conversation.

## Validation

- Read `./target/debug/asd --help`, `wait --help`, `agent --help`,
  `agent hook --help`, `agent explain --help`, `agent reload --help`, and
  `agent clear --help`; documented command names and options match.
- `node` link and JSON validation passed for the five Markdown documents with
  relative links and both hook configuration examples.
- `git diff --check` passed.

No Cargo workspace gate was run here because the coordinating task was already
running the final coverage and verification work. No product or test file was
edited by this documentation task.
