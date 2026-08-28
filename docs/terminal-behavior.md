# Terminal client behavior

This document covers the non-obvious contracts shared by `asd attach`,
`asd ui`, the daemon terminal model, and the host terminal. The desktop GUI
renders raw PTY bytes with ghostty-web and has a separate implementation guide
in [`crates/asd-dioxus/README.md`](../crates/asd-dioxus/README.md).

## Client roles and TUI viewer ownership

Ordinary `asd attach` and desktop GUI clients are shared: any number may view
and type into one session. `asd attach --read-only` is the same client with its
writing half removed: it receives the Snapshot and every Output, while the
daemon drops its `Input` and `Resize` and never enters it in size negotiation.
The CLI also stops sending those frames, so a watcher costs the session nothing.
This protects against typing into the wrong session, not against a client that
means harm — nothing stops that connection from using the attach-free scripting
frames, exactly as `tmux attach -r` cannot stop `tmux send-keys`. Only `asd ui`
has an exclusive interactive viewer slot. Selecting a session in a second TUI atomically displaces the first TUI;
the old process stays open, clears stale terminal contents, shows the ASD
placard, and may explicitly select the row again to take the view back.

Exclusivity is enforced in the daemon, not inferred from process IDs or UI
state:

- the handshake identifies a TUI client;
- every TUI Attach carries a fresh, nonzero `view_id`;
- the session stores the owner as `(client_id, view_id)`;
- `ViewRevoked` and `ViewRenamed` carry that stable view identity;
- replacing an owner removes it from membership, input capability, and size
  negotiation before granting the replacement;
- an ordinary shared client is never removed by a TUI takeover.

Names are display/routing state, not view identity. If an Attach races an
external rename, the request carries the name the client observed and the
daemon sends the canonical `ViewRenamed` before Snapshot. If the next
`SessionList` reaches the TUI before that rename event, the actor identifies
the same live session by its daemon-generated opaque instance identity, retags
its attach state, and emits the rename before forwarding the list. Duplicate
or stale rename/revoke events must match both `view_id` and the expected old
name before changing the view.

These rules prevent three failure modes: revoking a replacement that reused an
old name, dropping Output under the old label, and two TUI processes repeatedly
stealing the session back after a rename.

## Local VT rendering

`asd attach` and `asd ui` each maintain a local `GhosttyVt`. They feed daemon
Snapshot plus Output into that model and render cells themselves. This gives
each client independent scrollback position and selection while preserving
alternate-screen behavior.

libghostty-vt is an unstable dependency. All direct upstream calls belong in
`crates/asd-vt/src/ghostty.rs`; other crates use `VtBackend` and the fully
`Send` `RenderSnapshot` data type.

During `feed()`, terminal applications may issue DA/DSR or color queries. The
daemon-side VT drains `take_pty_responses()` and writes those bytes back to the
session PTY. A client-side mirror must drain and discard them: answering from a
renderer would create multiple responders on one shared PTY.

### Terminal appearance and color queries

CLI and TUI clients probe the real host terminal's OSC 10/11 foreground and
background while in raw mode and include the result in Attach. Appearance and
membership reach the session thread as one operation, so a query cannot race a
separate color update. For each color channel, the first non-unknown value is
locked for the shared session; unknown values are never replaced with a guessed
black or white theme.

The desktop GUI has no host terminal to probe. It reports the configured
ghostty-web theme through `TERMINAL_APPEARANCE` and remains an ordinary shared
client.

If the child asks for a default color before any real terminal has reported it,
the daemon holds that query in a bounded queue. Once the color is known, reply
using the same BEL, ST, or C1 ST terminator used by the request. The daemon is
the sole responder; client-facing output filters the query so local renderers
cannot answer it again. Default cells render through Reset/SGR 39/49 rather
than baking in an assumed palette.

## Exact snapshots

Upstream libghostty formatting treats history and the active screen as one line
stream and always trims trailing blank lines. Replaying that dump cannot
faithfully restore a screen whose bottom rows are empty; the cursor and history
land in the wrong places.

`snapshot_vt()` therefore produces two passes, following boo's
history-replay/repaint model:

1. select only History, emit it as content, then append one screen height of
   CRLF so the history is forced into scrollback;
2. clear/home, restore modes, select only Active, and repaint the screen using
   absolute positioning.

The method appends a final CUP because upstream formatting can emit tab-stop or
scroll-region state after its own cursor restore and move the cursor again.
Regression tests cover scrollback overflow, a cleared screen, and alternate
screen replay. Snapshot replay is the truth; callers must not reintroduce
resize jiggles, erase-below workarounds, or settle timers after switching.

## Mouse, selection, clipboard, and paste

At a shell prompt, local clients enable SGR mouse mode with button-event motion
(`1002` + `1006`). Mode `1000` is insufficient because it reports press and
release but not the drag positions required for text selection.

Selection is stored in absolute screen coordinates, not viewport coordinates.
Scrolling changes only the projection used to draw the highlight, so the
selection remains attached to its text. Releasing the drag copies through OSC
52. `Shift` plus drag bypasses client mouse capture for a host-native selection.

When the session application requests mouse tracking, mirror its exact DEC
tracking and encoding modes (`9`, `1000`, `1002`, `1003`, `1005`, `1006`,
`1015`, `1016`) to the host and forward reports unchanged. Host and PTY
coordinates are 1:1, so translating them would be incorrect.

Bracketed paste (`2004`) follows the application's current mode:

- `asd attach` mirrors mode 2004 so the host adds bracket markers and forwards
  the resulting bytes;
- `asd ui` receives `Event::Paste` after crossterm has stripped the host
  markers, so it conditionally restores them around the payload;
- do not add markers when the application did not request 2004, or they appear
  as literal text;
- remove an embedded closing marker from pasted content before wrapping it, so
  content cannot end the bracket early and turn its remainder into keystrokes.

## Host terminal restoration

The single restoration sequence is owned by `attach::restore_sequence` and is
shared by normal guards and terminating-signal handlers. It disables mirrored
mouse and paste modes, exits alternate screen, restores a visible default
cursor, and resets SGR.

`Drop` is not enough: SIGHUP, SIGTERM, and SIGINT can terminate a process while
the host is still in mouse capture or raw mode. Both CLI attach and TUI install
the platform terminating-signal restore before entering raw mode. Signal code
must use only async-signal-safe operations, restore cooked terminal attributes,
then restore the default disposition and re-raise so the exit status remains
correct. SIGKILL cannot be repaired; a user must run `reset` afterward.

After a normal CLI detach, the process exits directly once restoration and the
message flush are complete. Tokio's blocking stdin reader cannot be cancelled;
waiting for runtime teardown would otherwise hang until another Enter key.

Only show the host cursor when the rendered snapshot contains a visible cursor
position. A cursor scrolled out of the viewport has no position and must not
leave a host cursor artifact at the lower right.

## Size negotiation

One PTY has one size. The daemon computes columns and rows independently as the
minimum across all still-attached clients. This is deterministic and prevents a
large client from making output that a small client cannot display. Closing a
small client lets the PTY grow again. Followers do not participate, and neither
do read-only attachments: a watcher renders whatever size the session already
is, so opening one in a narrow window cannot reflow the work of the people
typing. A `Resize` from a read-only client is dropped for the same reason.

All detach paths, TUI revocation, slow-client removal, and failed direct sends
must remove the client's size record and recompute. A stale size entry is a
visible correctness bug, not harmless metadata.

The TUI reads its real host terminal size on every loop iteration rather than
trusting only `Event::Resize`. Crossterm installs its SIGWINCH behavior only
after the first event poll, so an early resize notification can be missed;
ratatui meanwhile lays out against the current kernel size. Treat resize events
as wakeups, not as the size source of truth.

Diagnostic distinction:

- if the entire TUI, including the sidebar, is squeezed into the upper-left,
  the host PTY's kernel winsize is stale (often SSH startup); moving the window
  causes a host `window-change` and corrects it;
- if the sidebar is correct but only session content is small, investigate asd
  size membership or rendering.

## TUI activity and tear-free rendering

Daemon activity is `idle_ms < IDLE_SETTLE_MS`. The TUI converts list samples
into per-session monotonic deadlines so running shimmer stops exactly at the
settle threshold instead of waiting for the next 1.5-second list poll. Real
Output for the active session extends its local deadline. An older concurrent
list sample must never overwrite that newer local observation.

The running-row shimmer changes only foreground hue. Animation-only writes are
limited to one frame every 500 ms while input polling stays at 30 ms. The pane
caches a complete snapshot and rerenders only for real output, scroll, resize,
or session changes.

When the application holds DEC synchronized output (`?2026h` without the
matching `?2026l` yet), the TUI continues displaying the previous complete
pane. A bounded fallback handles a missing close marker. This avoids sampling
half-drawn status bars from rapidly repainting applications.

## Windows Terminal URL detection

Windows Terminal automatically detects URLs and caches their screen
coordinates. During continuous redraw, text may move while its clickable area
temporarily remains at the old coordinates. This is host state, not an OSC 8
hyperlink emitted by asd.

The 500 ms animation interval gives the host's trailing debounce time to rescan
after animation-only output. That alone cannot guarantee a gap while the pane
itself is producing output, so the TUI also tracks the physical footprint of
`http`, `https`, `ftp`, and `file` URLs, including soft-wrapped continuations.

When a URL moves or disappears after the host has had a quiet interval, the TUI
clears the same-size ratatui viewport and performs a full repaint inside the
current alternate screen. It deliberately does not switch out of and back into
alternate screen: remote xterm-style hosts can expose that switch as a visible
full-screen flash. The host may keep an old auto-detected click target until its
own scanner catches up, but screen contents and explicit OSC 8 links remain
correct. Avoid repeating the full repaint until another quiet footprint
transition requires it.

The tradeoff is deliberate: preserving the host buffer avoids the flash and
does not discard its alternate-screen state, while accepting that an
auto-detected click target can lag. There is no VT sequence that directly
commands Windows Terminal to refresh only its URL regex cache.
