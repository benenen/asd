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

`wait --text` polls rendered screen content. `wait --idle` polls session
metadata. `follow` is different: it subscribes once and receives Output and
activity status in a single ordered stream.

Followers are stored separately from attached clients. They receive no
Snapshot, do not contribute to `attached_clients`, do not affect PTY size, and
cannot send attached-client operations. A new follower first receives current
status; each later PTY batch produces Output followed by current status.

The transition to idle occurs because no bytes arrive. Computing `running`
immediately after a PTY batch always yields true, so a session with followers
must use `recv_timeout` for the remaining settle interval while idle has not
yet been announced. On timeout it emits `running: false`; an `idle_announced`
guard prevents duplicate notifications and busy loops. With no followers, the
session returns to an ordinary blocking receive with no timer cost.

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
