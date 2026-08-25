# Session managers worth stealing from

Date: 2026-08-25
Status: research only — no code changed

## Scope and method

The question: asd manages concurrent agent sessions (one pty per session, a
daemon plus TUI/CLI/GUI clients). Which open-source tools solve the same
problems better, and what specifically should asd take?

Compared along the six dimensions that matter for this product: lifecycle,
restore/portability, multi-client attach, state visibility, replay/audit, and
expiry. Every "asd today" claim below was read out of the tree at `213a293`,
not recalled. External claims are from primary docs (see Sources); where a
claim could not be verified it says so.

## 0. asd today

| Dimension | asd at 213a293 |
|---|---|
| Lifecycle | `new` / `kill` / rename; a session ends when its child is gone (pty EOF on unix, child-exit watch on Windows). Daemon restart recreates every persisted session. |
| Restore | `sessions.tsv` holds `{name, cwd}` only. Restore spawns a **fresh default shell** `cd`'d there — `server.rs` passes `cmd = None`. The command that made the session is lost. cwd is re-sampled every 5s. |
| Multi-client | Shared clients (CLI attach, GUI) plus **one** exclusive TUI view; a second `asd ui` displaces the first. `follow` subscribers get Output without Snapshot, without counting as attached, without affecting size. Pty size is the per-axis min over attached clients. A client whose queue passes 4 MiB is dropped. |
| Visibility | `list` (+`--json`), `peek` (+`--scrollback`, `--json`), `inspect`, `follow`, `wait --until <state>`; agent state (working/blocked/idle/unknown) derived from screen text by per-agent TOML rules with checked-in fixtures; host metrics in the TUI bar. |
| Replay/audit | None. `peek --scrollback` returns the *current* buffer; there is no recording, no timeline, no export. |
| Expiry | None. Nothing is ever reaped for being idle, and `sessions.tsv` resurrects everything on the next daemon start, forever. |

Three of those six rows are empty or thin. That is where the borrowing should
concentrate.

## 1. Terminal multiplexers

### tmux

What matters here, quoted from the manual:

- `attach-session -r`: "the client is read-only ... only keys bound to the
  detach-client or switch-client commands have any effect."
- `new-session -t` (session *groups*): "Sessions in the same group share the
  same set of windows"; "The current and previous window and any session
  options remain independent and any session in a group may be killed without
  affecting the others."
- `pipe-pane`: "Pipe output sent by the program in target-pane to a shell
  command or vice versa."
- `capture-pane -p -S`: dump the buffer, negative line numbers reach into
  history.
- control mode (`-C`/`-CC`): a machine-readable protocol so another program can
  drive tmux — how iTerm2 renders native tabs over a remote tmux.

**Worth taking.** Read-only attach, and session groups.

**Implication for asd.** Read-only attach is close to free: input is already
validated per client at the session thread's membership check, so a viewer that
declares itself read-only just never passes that check. It also should not
join size negotiation — the machinery for "receives output, does not resize"
already exists for `follow`. The demand is real for an agent supervisor: watch
what the agent is doing on a shared box without the risk of a stray keystroke
landing in the agent's prompt.

Session groups answer a rough edge asd has today: two `asd ui` clients fight
over one session because the second displaces the first. tmux's model — shared
content, independent view state — is what "let two people watch the same agent
with independent scroll positions" would look like. asd already carries a
`view_id` per TUI attachment, so the seam exists.

`pipe-pane` is the poor-man's version of the recording story in §5; asd should
do the structured version instead.

### GNU screen

`multiuser on` plus `aclchg <user> -w "#"` gives a named user write-stripped
access to a session, and `screen -x` attaches several clients at once. Same
feature as tmux's `-r`, but *per user* rather than per attach — the observer
cannot promote themselves.

**Implication for asd.** Only relevant if asd ever grows real multi-user
sharing (today the socket/pipe is per-user and access is filesystem
permissions). Worth remembering as the next step after read-only attach, not
before it.

### dtach / abduco

Deliberately tiny: hold the pty, let clients come and go, nothing else. No
config, no state, no persistence across reboot.

**Implication for asd.** Nothing to take — this is asd's floor, already
cleared. Their value here is as a reminder that the daemon's core should stay
this small while the features below land in layers around it.

### zellij

The most directly relevant multiplexer, because it already solved the restore
problem asd punts on. From the session-resurrection docs:

- Serializes **layout, the running command per pane, and tab order** to a
  human-readable file in the cache dir, roughly every second.
- Re-running a restored command is **gated**: the pane shows "Press `ENTER` to
  run..." so a resurrect never silently re-executes something destructive.
  `--force-run-commands` opts out.
- Pane viewport and scrollback serialization are opt-in
  (`pane_viewport_serialization`, `scrollback_lines_to_serialize`, `0` = all).
- Exited sessions stay listed and resurrectable; `delete-session` /
  `delete-all-sessions` are explicit verbs.
- 0.43 added a web client: share a session to a browser URL, several clients
  each with their own cursor, bookmarkable and persistent; 0.44 added attaching
  to a remote session over HTTPS from a local terminal.

**Worth taking.** The confirm-gated command restore, the human-readable
serialized state, and "exited sessions are a listed state with an explicit
delete verb".

**Implication for asd.** This is the fix for asd's weakest row. asd persists
`{name, cwd}` and restores a bare shell, which for an agent daemon means the
restore throws away the only thing that mattered — that this session *was*
`claude`, in that worktree. Zellij shows how to restore the command without
the danger: record it, and on restore leave it staged behind a keypress rather
than running it. The same file, being human-readable, doubles as the session
definition file of §1/tmuxp.

The web client is a different product decision — asd already answers "see it
from elsewhere" with the GUI plus SSH remotes. Noted, not recommended.

### tmuxp / teamocil

Declarative sessions in YAML: windows, panes, `shell_command`,
`start_directory`, `before_script`. The interesting verb is `tmuxp freeze`:
export a *running* session to a workspace file, so the declaration is captured
from reality instead of hand-written.

**Implication for asd.** asd already has every primitive (`new --cmd --cwd`),
and no way to say "bring up my six agents in these six worktrees" in one
command. A workspace file plus `asd up` would be a small, high-leverage
addition, and `asd freeze` — write the current session set out as that file —
is what makes people actually keep the file up to date. Pair it with the
zellij-style restore file so there is one format, not two.

## 2. Agent-native tools

### herdr (github.com/herdrdev/herdr)

Already surveyed twice; asd's `detect/` module credits it as prior art for
screen-derived agent state. What is still unborrowed:

- **State vocabulary with a "seen" axis**: idle / working / blocked / **done** /
  unknown, where `done` is "finished while you were not looking" and only UI
  focus marks a session seen — CLI reads deliberately do not.
- **Notifications**: sounds plus terminal notifications, and a CLI group for
  them, so a blocked agent reaches the human instead of waiting to be polled.
- **An agent skill shipped inside the binary** (`skills/herdr/SKILL.md`) that
  teaches an agent to drive the tool: verify you are inside it, read IDs from
  JSON rather than predicting them, do not run the bare command to explore.
- **A JSON-schema'd socket API** with subscriptions and `events.wait`,
  including regex matching on output.
- **A vendored-dependency patch register** (`vendor/*.patches.md`: reason,
  remove-when, verification command).

**Implication for asd.** The `done`-vs-`idle` distinction is the one design
idea here that asd cannot express at all today, and it is exactly the multi-
agent supervision problem: with eight sessions, "idle" tells you nothing about
which ones finished something while you were in another tab. Notifications are
the delivery half of the same idea. The skill is cheap and asd's automation
surface is arguably better than herdr's already — it just has no document that
tells an agent so.

### Claude Code

Its session model is the most developed of anything surveyed:

- Sessions are named, resumable by name or id (`--continue`, `--resume`), with
  a picker that supports search, preview, rename, and widening scope (current
  worktree → repo → machine).
- Duplicate names are auto-suffixed rather than allowed to collide.
- **Branching**: `/branch` or `--fork-session` copies the conversation and
  leaves the original intact.
- **Checkpointing**: a snapshot per user prompt, the 100 most recent kept, saved
  *with* the conversation so `/rewind` survives a resume; the rewind menu
  restores code, conversation, or both, independently.
- **Retention**: checkpoints and sessions are swept after 30 days,
  configurable via `cleanupPeriodDays`.
- Scripts get structured access: `--output-format json`, and hooks receive a
  `transcript_path` so a `SessionEnd` hook can archive the transcript.

**Implication for asd.** Three things transfer. (1) The **retention sweep** —
a default expiry with a config knob is the shape asd's missing expiry row
should take. (2) The **picker ergonomics** — search, filter, and preview
without attaching; asd has `peek` already, so a preview pane in the TUI sidebar
is mostly UI work. (3) **Hooks as the integration surface** — "run this command
when a session's state changes" is how a supervisor tells a human or another
agent something happened, and it composes with anything.

Checkpoint/rewind itself does **not** transfer: it works because the
conversation is data Claude Code owns. asd is not in the edit path and should
not pretend to be. Branching does not transfer either — see §7.

### OpenHands

Event-sourced conversations: every interaction is an event, the log is
append-only and is the single source of truth, and replaying it reconstructs
the conversation. Persistence is a `base_state.json` plus an `events/`
directory (and, in the SDK, an append-only JSONL per conversation); restore is
"same id, same persistence dir". Trajectories can be replayed.

**Implication for asd.** The pattern, not the plumbing: a session's history
should be an append-only log that can be replayed, rather than only a live
buffer that can be peeked. asd already produces exactly the right events
(output bytes, resizes, timestamps) and currently throws them away. §5 is the
concrete form this should take.

### Codex CLI

Same family of answers as Claude Code — resume by session id, rollout files on
disk. Nothing additional worth borrowing beyond what is already listed;
recorded here so the survey is not read as incomplete. *(Not verified against
primary docs in this pass.)*

## 3. Programmable terminals

kitty (`kitty @ ls` returning a JSON tree, `kitty @ send-text`, a socket
enabled by `allow_remote_control`) and WezTerm (`wezterm cli list --format
json`, `cli spawn`, `cli send-text --pane-id`, a mux server with local and SSH
domains) both expose the terminal as a scriptable service.

**Implication for asd.** asd is already this, and in places better: `send`,
`peek --json`, `wait --until`, `follow`, `inspect`. The gap is not capability
but *contract*: asd's protocol is private postcard, and the CLI is the only
supported surface. If asd ever wants third-party integrations, the missing
piece is a documented, versioned JSON surface — most cheaply an `asd events
--json` stream of state transitions rather than a whole second protocol.

## 4. Schedulers and fleets

dstack reclaims idle instances after `idle_duration` (default 3 days), never
below the fleet's `nodes.min`, with `off` to disable and `0s` to reclaim
immediately. Slurm's equivalent is a hard time limit per job plus accounting
records that outlive the job.

**Implication for asd.** This is the missing expiry policy, and dstack's knob
shape is the right one to copy: a duration, a way to disable it, and a
protected class that is never reaped (asd's equivalent of `nodes.min` is
"sessions the user pinned", or simply "sessions with a client attached").
Slurm's accounting half is the audit story in §5 again — the record should
outlive the session.

## 5. Recording and replay: asciicast v2

Newline-delimited JSON: one header object (`version`, `width`, `height`,
`timestamp`, `duration`, `idle_time_limit`, `command`, `title`, `env`, `theme`)
followed by `[time, code, data]` events, where code is `o` output, `i` input,
`r` resize (`"WIDTHxHEIGHT"`), or `m` marker. It is append-only precisely so it
can be written incrementally, survive a crash, and be streamed live.

**Implication for asd.** This is the cheapest large win available. asd's
session thread already sees every byte, every resize, and a monotonic clock;
writing a `.cast` per session behind a config flag turns "what did the agent do
for the last hour" from unanswerable into `asciinema play`. The format's
header fields line up with what asd already knows (command, title, size, env),
and its append-only design matches asd's crash-safety needs. Bounded by the
same kind of knob as scrollback, and off by default.

## 6. Recommendations, ranked

| # | Change | Why now | Cost | Lands in |
|---|---|---|---|---|
| P0-1 | Persist the spawn command; restore it **staged behind a confirm**, zellij-style | Restoring a bare shell throws away the only thing an agent session was | S — one column in `sessions.tsv`, one flag on restore | `asd-daemon/store.rs`, `server.rs`, config |
| P0-2 | Read-only attach (`attach --read-only`, and a viewer kind) | Watching an agent without being able to typo into it; the enforcement point already exists | S — client kind + membership check + skip size negotiation | proto (new kind → version bump), `conn.rs`, `session.rs` |
| P0-3 | Idle expiry with a config knob and a protected class | Sessions accumulate forever and are resurrected forever | S–M — a timer plus a policy; `asd prune` for the persisted list | `asd-daemon/registry.rs`, config |
| P1-4 | Per-session asciicast v2 recording, off by default | Replay and audit for free from bytes asd already has | M — a writer on the session thread, rotation/size policy | `asd-daemon/session.rs`, config |
| P1-5 | Workspace file + `asd up` / `asd freeze` | "Bring up my six agents" is currently six commands and no record | M — file format plus two CLI verbs; no protocol change | `asd-cli` |
| P1-6 | State-change hooks (run a command when a session becomes blocked/done) | Turns polling into notification; the detection already exists | M — hook config, spawn discipline, no protocol change | `asd-daemon` |
| P2-7 | A `done` state with a "seen" axis | Distinguishes "finished while you were away" from "idle"; the multi-agent case | M — state vocabulary is on the wire, so a version bump, plus a seen-marking rule | proto, daemon, TUI/GUI |
| P2-8 | TUI picker ergonomics: filter, search, preview-without-attach | Cheap ergonomics on top of `peek` | S–M | `asd-tui` |
| P2-9 | Independent views of one session (tmux session groups) | Removes the "second `asd ui` displaces the first" rough edge | M–L — per-view scroll/size state | daemon + TUI |
| P2-10 | Documented JSON event stream for third parties | Only if integrations become a goal | M | `asd-cli` |

P0-1 and P0-3 are the same file and the same restart path; doing them together
is cheaper than doing either alone. P0-2 is independent and self-contained.

## 7. Deliberately not borrowing

- **Panes, splits, layouts** (tmux, zellij, herdr). asd's stated boundary is one
  pty per session, no panes. Every layout feature surveyed assumes the opposite.
- **Plugin systems and marketplaces** (zellij, herdr). A plugin surface is a
  second product; asd's extension point is the CLI plus hooks.
- **Forking or branching a live session** (Claude Code `/branch`). A
  conversation is data and can be copied; a pty's process state cannot. The
  honest equivalent — "new session, same cwd and command" — is `asd new`.
- **Checkpoint/rewind of the working tree** (Claude Code). asd does not make the
  edits and should not own undoing them; git does.
- **Browser sharing and multiplayer cursors** (zellij 0.43/0.44). asd already
  answers remote viewing with the GUI and SSH remotes; this is a large build for
  an overlapping outcome.
- **Per-user ACLs** (screen `multiuser`). Premature until asd has a real
  multi-user story; today the socket is per-user.

## 8. Where asd is already ahead

Worth stating so the list above is not read as a deficit report. asd's
exclusive-TUI-plus-shared-clients split, per-axis size negotiation, and
follow-is-not-attach distinction are more carefully specified than the
equivalents in any multiplexer surveyed. Its automation surface (`send`,
`peek --json`, `wait --until <state>`, `follow`, `inspect`) is richer than
kitty's or WezTerm's remote control for the agent-supervision case, and its
agent-state detection is rule-based, per-agent, and pinned by checked-in
fixtures rather than hardcoded. The single-binary, no-panes shape is a feature.

## 9. Open questions

- Does an idle timeout want to kill the child, or only drop the session from
  the persisted list so it stops being resurrected? The second is much safer and
  may be all that is needed.
- If recording lands, is the unit a session or a session-run? A restored session
  is arguably a new recording with a link to the previous one.
- `done`-with-a-seen-axis needs a decision about *what* marks seen: asd has no
  focus concept in the CLI, and the TUI's exclusive view is the only thing that
  resembles one.

## Sources

- tmux manual (attach-session `-r`/`-d`/`-E`, `new-session -t`, `pipe-pane`,
  `capture-pane`, control mode): https://man7.org/linux/man-pages/man1/tmux.1.html
- GNU screen `aclchg` and multiuser sharing:
  https://www.gnu.org/software/screen/manual/html_node/Aclchg.html and
  https://aperiodic.net/screen/multiuser
- zellij session resurrection: https://zellij.dev/documentation/session-resurrection
- zellij web client and remote sessions: https://zellij.dev/tutorials/web-client/,
  https://zellij.dev/news/web-client-multiple-pane-actions/,
  https://zellij.dev/news/remote-sessions-windows-cli/
- tmuxp freeze: https://tmuxp.git-pull.com/cli/freeze.html
- Claude Code checkpointing: https://code.claude.com/docs/en/checkpointing
- Claude Code sessions: https://code.claude.com/docs/en/sessions
- OpenHands event storage and replay:
  https://deepwiki.com/All-Hands-AI/OpenHands/12.2-event-storage-and-replay,
  https://docs.openhands.dev/sdk/guides/convo-persistence
- dstack fleet idle policy: https://dstack.ai/docs/concepts/fleets/
- asciicast v2 format: https://docs.asciinema.org/manual/asciicast/v2/
- herdr: https://github.com/herdrdev/herdr (read from a local clone at 6e8b138)
