# Automation and observation semantics

This document records the internal guarantees behind non-attached commands.
The command-line syntax and examples in the repository
[`README`](../README.md) remain authoritative.

## Design boundary

`send`, `peek`, `wait`, `follow`, `inspect`, and `card` let scripts and agents
operate without becoming attached render clients. They must not change PTY
size, consume a TUI viewer slot, or require callers to reconstruct daemon state
from terminal escape sequences.

## `send`

`SendInput` enters the session thread as scripted input rather than attaching a
client. Named keys and bytes are encoded by the client, but ordering with PTY
activity and other input is enforced by the session thread.

`--enter` is one atomic operation: write the payload, wait for 300 ms of
session-thread input quiet, then write carriage return before acknowledging.
No concurrent input may be inserted between payload and Enter. This avoids an
agent TUI interpreting a pasted newline as content instead of submission.

## `peek` and history limits

`Peek` renders terminal state inside the daemon and returns a screen plus the
requested scrollback. `Scrollback` has three states:

- `None`: active screen only;
- `All`: all retained history plus the active screen;
- `Lines(n)`: at most the last `n` history rows plus the active screen.

The daemon applies the line limit before encoding the frame. Client-side
truncation is not acceptable: a session can retain tens of thousands of rows,
and sending all of them only to discard most can exceed the 4 MiB frame cap.
For `Lines(n)`, calculate the starting history row from retained scrollback;
the active screen is always included. A value larger than retained history is
equivalent to `All`.

## Activity, `wait`, and `follow`

The shared definition of activity is
`idle_ms < asd_proto::IDLE_SETTLE_MS` (currently a two-second settle period).
`SessionInfo.running`, `wait --idle`, TUI shimmer, and `FollowStatus` all derive
from this constant.

`wait --text` and `wait --regex` register session-local screen conditions using
`WaitForScreen`. The owner checks the current visible screen and, if unmatched,
registers the condition in the same session-thread turn. After every PTY feed,
it checks all pending conditions against that observed post-feed screen before
processing another PTY batch. A match survives a later erase. Bytes written and
erased within a single feed are not separately observed. History is excluded,
using the same plain-text rendering and trailing-blank trimming as `peek`
without scrollback. Matches return the exact session identity, including after
a rename.

Regular expressions use Rust `regex` syntax. CLI validation happens before
connection; daemon compilation runs outside the session thread. Both enforce
4 KiB of pattern input and a 1 MiB compiled regex size limit; the regex DFA
cache is also limited to 1 MiB. Each session allows 64 active screen waits.
Match, timeout, exit, and disconnected reply receivers release registrations;
disconnect cleanup explicitly wakes even a silent session. Waiters do not
attach, affect PTY size, or consume a viewer slot. A current-screen match can
succeed with a zero timeout; otherwise the nearest deadline wakes the session.
CLI success remains exit 0, timeout exit 4, and missing session exit 3 with the
same wording in every wait mode.

`wait --idle` and `wait --until` subscribe to session events, resolve the initial
name to an identity in the opening snapshot, and follow that identity across
renames. A dropped connection resumes from the last accepted cursor; an invalid
cursor forces a reset. Reconnection retains the original timeout deadline and
never switches to a same-name replacement. All four wait modes are polling-free.
`follow` subscribes separately and receives Output and activity status in a
single ordered stream.

Followers are stored separately from attached clients. They receive no
Snapshot, do not contribute to `attached_clients`, do not affect PTY size, and
cannot send attached-client operations. A new follower first receives current
status; each later PTY batch produces Output followed by current status.

The transition to idle occurs because no bytes arrive. Computing `running`
immediately after a PTY batch always yields true, so every session uses
`recv_timeout` for the remaining settle interval while idle has not
yet been announced. On timeout it emits `running: false`; an `idle_announced`
guard prevents duplicate notifications and busy loops. The receive timeout is
the earliest idle, deferred detection, screen-wait, or one-second foreground
metadata refresh deadline. Idle events therefore work even with no followers.

Session exit sends both `FollowStatus { running: false }` and the session-exited
error. Default follow may stop on the status transition; `--forever` ignores
idle and needs the exit error as its terminal event.

That last status is also the only one carrying `exit`: the child's code, and the
platform's name for the signal when one ended it. It has to ride there rather
than on `SessionInfo`, because by the time the status is known the session has
left the registry and no `list` can report it. `follow --json` puts it on the
terminal `exit` event as `code` and `signal`; the session-exited error names it
in prose for whoever is only reading messages, `asd attach` included. A signal
name is the platform's wording (`Hangup`, `Killed`), not a `SIG*` constant.

## Sequenced session events

`SubscribeEvents { after, wants_notifications }` dedicates a handshaken
connection to metadata events. It receives `EventStreamStarted`, optional
replay, then live `SessionEvent` frames. This connection does not attach,
follow PTY output, negotiate size, or take a viewer slot.

Cursors contain a random daemon-process epoch and monotonically increasing
sequence. An absent, foreign, expired, or future cursor receives a sorted full
snapshot with `reset: true`. A recoverable cursor receives `reset: false`, an
empty sessions vector, and every event after that cursor. Replay retains 512
events; the live queue holds 64 frames. Overflow closes after its contiguous
queued prefix, allowing the client to reconnect and replay or reset.

Events carry registration, identity-keyed name-free patches, canonical rename,
and exit. Activity emits only started/settled edges, not every output batch.
Snapshot `idle_ms` reads the latest output timestamp without consuming cursor
space. State changes caused by manifest reload carry `DetectorReload`, distinct
from screen-driven changes. `asd-client::events::EventFeed` checks epoch and
strict consecutive cursors before applying events; consumers use its accepted
`last_cursor()` with the returned change.

Only handshaken GUI and TUI clients can request notification leases. Each class
has at most one holder; CLI/proxy requests receive no lease. Dropping a
subscription or closing its socket transfers the lease to the oldest waiting
subscriber of that class, including while the daemon is otherwise quiet.

## Modelled follow output

Normal `follow` output is passed through a client-side `GhosttyVt`; `--raw`
is the explicit verbatim-byte escape hatch. Stripping ANSI is not a substitute
for terminal modelling because the escape sequences carry the information that
distinguishes new output from a repaint.

The model divides terminal rows into two categories:

- rows below `scrollback_rows()` have left the active screen and can never be
  changed again; emit them once, in order, as committed `output` events;
- rows still in the active screen can be redrawn; emit the current `screen`
  only at settle, exit, or timeout, and suppress a screen identical to the
  previous one.

Consequences are intentional:

- short output that never scrolls off screen arrives in the settled `screen`,
  not line-by-line;
- alternate-screen programs such as vim, htop, and less commit no history and
  therefore produce only `screen` snapshots;
- status is recorded only when `running` flips, even though the daemon sends a
  status frame after every output batch.

The client terminal size must match the session's PTY size or wrapping and row
identity become wrong. `follow` obtains the size with one `ListSessions`
request before subscribing; if the session is absent it uses a harmless
80x24 fallback and lets `Follow` return the canonical no-such-session error.
The current-thread runtime is required because the local VT is `!Send` across
awaits. Raw mode still uses a UTF-8 stream decoder so multibyte characters split
across batches are not corrupted.

## `card`

`asd card` is a client-side session-selection aid for agents. It deliberately
uses a three-step information ladder:

1. `list` reports each local session directory and available project docs;
2. `inspect` adds titles and bounded opening summaries;
3. `cat` reads one requested file in full.

This keeps session selection from loading several full READMEs into context.
The recognized project documents are `README.md`, `CLAUDE.md`, `AGENTS.md`, and
`CONTRIBUTING.md`, matched case-insensitively while returning the spelling that
actually exists on disk. If both case variants exist, exact spelling wins;
otherwise a stable sorted choice wins rather than directory iteration order.

`card` is intentionally local-only. `ListSessions` supplies the session PID and
the client reuses `asd_daemon::read_cwd(pid)`; a remote PID has no meaning on
the local machine and may collide with an unrelated process. If cwd cannot be
established, report it as unknown rather than guessing. macOS currently has the
same honest fallback until cwd lookup gains a libproc implementation.

`cat` resolves relative to the session directory. It first tries the exact
path, then walks components case-insensitively. Fuzzy traversal is downward
only: absolute roots, drive prefixes, and `..` are rejected. The final
canonical path must remain inside the canonical session directory.

This is a guardrail against an agent wandering outside the project, not a
security boundary: the user running `asd card` already has ordinary filesystem
permissions to read those paths.

## What a session says about itself

`asd status --text "step 3/7: running tests"` sets one line on a session;
`asd list`, `list --json` (as `says`), `inspect` and the TUI sidebar show it.
With no name it uses `$ASD_SESSION`, so the program inside a session describes
itself without knowing its own name or where its daemon is — both are already
in its environment.

This is the only progress channel that does not go through reading the screen.
Detection can tell working from blocked because those look different; it cannot
tell step three from step four, because they look the same. So the two coexist
and neither overrides the other: `state` stays the daemon's reading, `says` is
the session's own claim, and a display prefers the deliberate one — the status
line, then the terminal title, then the command.

The daemon keeps the first 512 bytes and drops the rest: every `list` carries
this to every client, and the TUI polls the list every 1.5s. It is not
persisted either — a restored session is a new process, which can say what it
is doing when it knows.

## Prompting another session

`asd ask <name> "<text>"` is `send` and `wait` as one operation, plus three
things the pair cannot do on its own:

- **It refuses to type into a session that is already blocked.** A session
  parked on `Do you want to proceed? (y/n)` will take whatever arrives as the
  answer to *that* question. `ask` reads the state first and exits 5 rather than
  answering someone else's dialog by accident. Answering one deliberately is
  what `send` is for.
- **It gives up early when nothing comes back at all.** A full-screen program
  that never reads its input swallows the text silently, and waiting the full
  timeout for a settle that cannot come is a waste. Any state change, or any
  output newer than the session's last output when the prompt went in, counts as
  having received it; five seconds of neither is reported as a stall. The guard
  measures against that starting age rather than against zero, because a session
  can answer faster than the acknowledgement for the prompt travels back, and
  comparing with zero then makes an instant answer look like silence.

  Silence is all the guard can test. A tty in cooked mode echoes what is typed
  into it whether or not the program reads it, so a `sleep` with echo left on
  looks like it received the prompt and `ask` falls through to the activity
  rule. The programs that actually eat input turn echo off, which is where the
  guard does fire.
- **It says where it settled**, so a caller can branch on `blocked` without
  asking again.

Settling means `Idle` or `Blocked`. `Unknown` — a plain shell, or any program
without detection rules — falls back to the activity rule `list` uses to print
"idle": no bytes for the settle interval. `--until` overrides the whole thing
and waits for exactly one state.

## Agent detection diagnostics

`asd agent explain NAME [--json]` requests an explanation from the session's
own terminal thread. JSON is the protocol `DetectionReport` object, including
`foreground_command`, `candidate_manifest_ids`, `selected_manifest_id`,
`state`, all evaluated `rules`, and `generation`. Each rule includes its
manifest and rule identifiers, priority, state, region, match result, evidence,
and non-match reason. Text output renders those same fields.

`asd agent reload [--json]` rereads the daemon's manifest directory. Valid
overrides replace rules wholesale; invalid edits retain the previous valid
override and report a diagnostic. Deleting an override returns to the embedded
rules. Existing quiet sessions immediately reevaluate their current screen.
Neither command needs `ASD_SESSION_ID` or an attachment.

Reload JSON is an object with `generation`, `diagnostics`, and
`pending_identities`. The installed generation stays active even when some
session threads have not acknowledged within five seconds. Empty pending
identities means exit 0; a non-empty list is printed before exit 1. Delayed
session threads still apply their queued generation unless they have already
applied a newer one. Invalid-file diagnostics alone do not imply a partial
session barrier or failure exit status.

## Authoritative agent resume hooks

Configure the agent's lifecycle hooks explicitly to invoke these commands;
each command reads the vendor JSON payload directly from stdin:

```bash
asd agent hook codex start
asd agent hook codex end
asd agent hook claude start
asd agent hook claude end
asd agent clear
```

Hooks use `ASD_SESSION_ID`, not the mutable session name. The daemon provides
this variable in every session; it survives rename but changes on replacement
or restore. `clear` explicitly removes the hosting session's resume metadata.
No command edits Codex or Claude configuration files.

Payloads require string `session_id` and `hook_event_name` (`SessionStart` or
`SessionEnd`, matching the requested phase). Start requires string `source`:
Codex accepts `startup`, `resume`, `clear`, `compact`; Claude also accepts
`fork`. End requires string `reason`: Codex accepts only `other`; Claude accepts
`clear`, `resume`, `logout`, `prompt_input_exit`, `other`. Extra JSON fields are
ignored. References must match `[A-Za-z0-9][A-Za-z0-9_-]{0,127}` exactly; the
CLI limits the full payload to 64 KiB.

Start requires proof from the foreground process's actual executable and raw
argument boundaries. Display strings and shell `-c` source never prove a live
agent. On platforms without process lookup, the recorded launch is a fallback
only when it is a conservative single invocation without quotes, operators,
substitutions, or control characters; manually launched agents on Windows may
therefore be rejected. A failed lookup on a supported platform rejects Start.
End requires the exact stored kind/reference, even if the process has exited.
Duplicate references owned by another live or retained session are rejected.
A successful hook returns only after durable persistence; invalid, stale, or
failed writes exit non-zero without changing in-memory resume metadata.

Restart starts a fresh shell and stages `codex resume ID` or
`claude --resume ID` without Enter. `--run-restored-commands` opts into execution
of the same validated command. Duplicate saved claims select the newest daemon
timestamp, then ascending session name; every loser stages its original launch
command without Enter, even with opt-in execution. Any unconfirmed original
containing control characters (including TAB, CR, LF, and ESC) is left unstaged
with a warning; its persisted bytes are retained. Printable commands stage
exactly, and explicit execution opt-in retains the original command behavior.
Missing agent executables
remain visible shell errors rather than silently starting a fresh conversation.
