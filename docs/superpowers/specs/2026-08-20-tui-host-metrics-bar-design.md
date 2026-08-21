# Host metrics in the `asd ui` bottom bar

Date: 2026-08-20
Status: implemented

## Goal

Show the daemon host's CPU, memory, and network throughput in the `asd ui`
bottom bar, each behind an emoji icon, alongside the clock that is already
there. Segments are coloured so the eye can separate them.

## Why the daemon host, not the local one

`asd ui` can attach to a daemon on another machine. The load that matters is
the load on the machine actually running the sessions, so the numbers are
sampled by the daemon and sent over the wire.

The clock stays local. It reads `SystemTime::now()` in the client today
(`asd-tui/src/lib.rs`, `now_ms`), despite `bar.rs` naming its helper
`server_time_at` and calling it a "server clock" — the name is wrong, the
behaviour is local wall time. Changing it is out of scope here, so the bar will
show a local time next to remote metrics. That inconsistency is accepted
deliberately rather than overlooked.

## Protocol

`PROTO_VERSION` goes 14 → 15. Two frames, named after the existing
`Peek`/`PeekReply` pair:

```rust
Frame::HostMetrics,                                    // client asks
Frame::HostMetricsReply { sample: Option<HostSample> } // daemon answers
```

```rust
pub struct HostSample {
    /// Whole-host utilisation, 0-100, averaged across cores. An integer
    /// because the bar renders a whole percent anyway, and because a float
    /// field would cost `Frame` its `Eq`.
    pub cpu_pct: u8,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    /// Bytes per second, summed over every non-loopback interface.
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
    /// How old this reading is. Relative, not a timestamp.
    pub sampled_age_ms: u64,
}
```

Two deliberate choices:

- **`Option`, not zeroes.** For the first second of a daemon's life the sampler
  has not produced a reading yet. `None` means "no reading"; the bar draws
  nothing. Zeroes would claim the host is idle.
- **Age, not a timestamp.** Two hosts need not agree on the wall clock, so an
  absolute time cannot be compared against the client's. An age is meaningful
  across machines. This version carries the field and renders the values
  unconditionally; dimming a stale reading is left for whoever needs it, so no
  staleness threshold is defined here.

Also required: add both frames to `all_frames()` in
`crates/asd-proto/tests/codec.rs` including its explicit version assertion, add
a v15 entry to the protocol module history in `crates/asd-proto/src/lib.rs`, and
run `cargo test -p asd-proto`.

## Daemon sampler

New module `crates/asd-daemon/src/metrics.rs`.

A worker starts with the daemon and wakes on a fixed 1000 ms timer. It holds a
`sysinfo::System` and a `sysinfo::Networks`, refreshes only what is used
(`refresh_cpu_usage`, `refresh_memory`, `Networks::refresh`), and publishes the
result into an `Arc<RwLock<Option<HostSample>>>`.

The connection handler answers `HostMetrics` by taking a read lock and cloning.
**It never samples.** That is the whole point of the worker: the sampling
cadence belongs to the worker's own timer, so no number of clients and no
fast-polling script can accelerate it. This is structural, and stronger than a
cache with a TTL.

CPU percentage needs two readings to difference. `sysinfo` does that internally
between refreshes, but requires at least `MINIMUM_CPU_UPDATE_INTERVAL` between
them; the fixed 1000 ms cadence satisfies it.

Network rate divides the byte deltas `Networks::refresh` reports by the
*measured* elapsed time, not by an assumed 1000 ms — a delayed thread would
otherwise report a rate that is too high. Loopback interfaces are excluded; all
other interfaces are summed.

`sysinfo` is cross-platform, so this feature adds no `#[cfg]` at any call site
and therefore no `platform/` module. A reviewer expecting one should not.

The daemon's Windows build can only be cross-checked locally
(`cargo check --target x86_64-pc-windows-gnu`), not run; see
`docs/cross-platform-development.md`.

## TUI wiring

`asd-tui/src/conn.rs` already polls `ListSessions` on a 1500 ms ticker. The same
tick also writes `Frame::HostMetrics`; `HostMetricsReply` becomes an
`Ev::Metrics(Option<HostSample>)`. `App` gains `metrics: Option<HostSample>`,
set from that event, marking the frame dirty. `bar.rs` renders it.

Polling at 1500 ms against a 1000 ms sampler means the bar never shows a reading
more than about a second old.

## Bar layout

The metrics join the existing left-hand group, after the keybind hint:

```
 Keybinds: Ctrl+A   🕐 2026-08-20 09:05:07   💻 12%   🧠 6.1G/31G   🌐 ↓1.2M ↑340K
```

The date keeps its current full `%Y-%m-%d %H:%M:%S` form.

Colours come from the existing palette in `asd-tui/src/ui.rs`; no new ones.

| Segment | Colour |
| --- | --- |
| Icons | `DIM` — the colour belongs on the values |
| 🕐 clock | `MUTED` — context, not status |
| 💻 CPU | `OK` below 70%, `ACCENT` 70-90%, `ALERT` above 90% |
| 🧠 memory | same thresholds, on used/total |
| 🌐 network | `MUTED`, with ↓ and ↑ separating receive from transmit |

Network gets no threshold colouring: there is no throughput that is "bad", and
colouring it red would raise an alarm that means nothing.

Numbers are formatted in binary units suffixed `K`, `M`, `G` (1024-based, the
convention `free -h` and `du -h` use), with one decimal below 10 and none above:
`6.1G`, `31G`, `340K`. CPU is a whole percent. Rates carry no `/s`; the arrows
already say it is a rate, and the bar is short on columns.

## Narrow terminals

Segments drop right to left as width runs out:

1. everything
2. drop network
3. drop memory
4. drop CPU
5. drop the clock (today's behaviour)
6. keybind hint alone, truncated (today's behaviour)

The keybind hint survives longest because it is the only actionable thing on the
bar. The clock outlives the new segments so that a narrow terminal keeps
behaving the way it does today.

Note that four double-width icons cost 8 columns before any digits, and the full
date is 19 more. On an 80-column terminal the left group will reach the drop
order quickly; that is expected, not a defect.

## Testing

- `asd-proto`: both frames in `all_frames()`, version assertion at 15.
- `asd-daemon`: the rate computation and the loopback filter are pure functions
  taking counter values, unit-tested without `sysinfo` — a byte delta over a
  measured interval, and `lo` excluded from the sum.
- `asd-tui`: the two existing `bar.rs` tests assert the exact prefix
  `" Keybinds: Ctrl+A  2026-08-14 09:05:07"` and must be updated. New tests
  cover byte and rate formatting, the threshold-to-colour mapping, and the drop
  order at several widths.

## Out of scope

- Sourcing the clock from the daemon.
- Per-core CPU, per-interface breakdown, disk, or load average.
- The Dioxus GUI's `sidebar-footer`, which has no clock and is not this bar.
