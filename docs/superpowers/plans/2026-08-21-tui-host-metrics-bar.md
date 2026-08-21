# Host Metrics Bottom Bar Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show the daemon host's CPU, memory, and network throughput — each behind an emoji icon, in its own colour — in the `asd ui` bottom bar next to the clock already there.

**Architecture:** A worker inside the daemon samples the host once a second and stores the reading on the `Registry`. A new request/reply frame pair lets a client read that stored reading; the request path never samples. `asd ui` asks for it on the 1500 ms ticker it already uses for `ListSessions`, and `bar.rs` renders the result as coloured segments that drop right-to-left when the terminal is narrow.

**Tech Stack:** Rust, `sysinfo` 0.38.4 (`default-features = false`, features `system` + `network`), tokio, ratatui, postcard.

**Spec:** `docs/superpowers/specs/2026-08-20-tui-host-metrics-bar-design.md`

## Global Constraints

- Write all code comments in English, including doc comments and comments in Cargo files.
- Any new protocol frame bumps `asd_proto::PROTO_VERSION` and updates `all_frames()` in `crates/asd-proto/tests/codec.rs`, including its explicit version assertion. Update the protocol module history. Run `cargo test -p asd-proto`.
- Both endpoints upgrade together; there is no multi-version compatibility.
- No `#[cfg(unix)]` / `#[cfg(windows)]` at call sites. `sysinfo` is cross-platform, so this feature adds none and creates no `platform/` file. A reviewer expecting one should not.
- `asd-client` and `asd-dioxus` stay free of PTY/process-management dependencies. This plan does not touch them.
- **The crates.io mirror (`rsproxy.cn`) is unreachable as of 2026-08-21.** Every cargo command in this plan must be run with `--offline`. `sysinfo` 0.38.4 is already in the local cargo cache; `cargo add --offline`, `cargo check --offline`, and `cargo check --offline --target x86_64-pc-windows-gnu` were all verified to work before this plan was written. If the network has returned, `--offline` is harmless and may be dropped.
- The clock in the bar stays local wall time. Do not change it.

---

### Task 1: Protocol — `HostSample` and the frame pair (v15)

**Files:**
- Modify: `crates/asd-proto/src/lib.rs` (history block ~line 30, `PROTO_VERSION` line 44, `Frame` enum)
- Modify: `crates/asd-proto/tests/codec.rs` (`all_frames()` line 11, version assertion line 196)

**Interfaces:**
- Consumes: nothing.
- Produces: `asd_proto::HostSample { cpu_pct: u8, mem_used_bytes: u64, mem_total_bytes: u64, net_rx_bps: u64, net_tx_bps: u64, sampled_age_ms: u64 }`, `Frame::HostMetrics`, `Frame::HostMetricsReply { sample: Option<HostSample> }`, `PROTO_VERSION == 15`.

- [ ] **Step 1: Write the failing test**

In `crates/asd-proto/tests/codec.rs`, add the two frames to the vector returned by `all_frames()` (append them just before the closing `]`):

```rust
        Frame::HostMetrics,
        Frame::HostMetricsReply {
            sample: Some(asd_proto::HostSample {
                cpu_pct: 12,
                mem_used_bytes: 6_500_000_000,
                mem_total_bytes: 33_000_000_000,
                net_rx_bps: 1_258_291,
                net_tx_bps: 348_160,
                sampled_age_ms: 740,
            }),
        },
        Frame::HostMetricsReply { sample: None },
```

And change the version assertion at line 196:

```rust
    assert_eq!(asd_proto::PROTO_VERSION, 15);
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --offline -p asd-proto`
Expected: FAIL — the compiler cannot find `Frame::HostMetrics`, `Frame::HostMetricsReply`, or `asd_proto::HostSample`.

- [ ] **Step 3: Add the type and the frames**

In `crates/asd-proto/src/lib.rs`, bump the version at line 44:

```rust
pub const PROTO_VERSION: u32 = 15;
```

Continue the history block. Every entry in it is a clause of one running sentence, so replace the final line

```rust
//! says whether bytes are arriving.
```

with

```rust
//! says whether bytes are arriving; v15 adds `HostMetrics`/`HostMetricsReply`,
//! letting a client read the daemon host's CPU, memory and network rates. The
//! daemon samples them on its own timer and answers from that reading, so the
//! request never measures anything and no client can drive the sampling rate.
```

Add the struct next to the other shared types (above the `Frame` enum):

```rust
/// One reading of the daemon host's resource use, taken by the daemon's own
/// sampler rather than measured when a client asks. Rates are per second.
///
/// Every field is an integer so that `Frame` keeps its `Eq`. CPU is the one
/// value the host reports as a float, and the bar renders it as a whole
/// percent anyway, so it is rounded at the sampler rather than carried at a
/// precision nothing consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSample {
    /// Whole-host utilisation, 0-100, averaged across cores.
    pub cpu_pct: u8,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    /// Bytes per second, summed over every non-loopback interface.
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
    /// How old this reading is, in milliseconds. Deliberately an age and not a
    /// timestamp: two hosts need not agree on the wall clock, so an absolute
    /// time could not be compared against the client's.
    pub sampled_age_ms: u64,
}
```

Add the two variants to the `Frame` enum, after `InspectReply`. Leave `Frame`'s existing derives alone — `HostSample` is all-integer precisely so that `Eq` still holds:

```rust
    /// client → daemon: what is the daemon host's resource use right now.
    HostMetrics,
    /// daemon → client: the sampler's most recent reading. `None` means the
    /// sampler has not produced one yet, which is true for the first second of
    /// a daemon's life. It is `None` rather than zeroes because zeroes would
    /// claim the host is idle.
    HostMetricsReply {
        sample: Option<HostSample>,
    },
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --offline -p asd-proto`
Expected: PASS, including the round-trip over every frame in `all_frames()`.

- [ ] **Step 5: Commit**

```bash
git add crates/asd-proto/src/lib.rs crates/asd-proto/tests/codec.rs
git commit -m "feat(proto): v15 carries the daemon host's resource sample

A client that wants to show the load on the machine actually running the
sessions has no way to ask for it today. Add the reading as its own frame
pair rather than folding it into SessionList, so \`asd list\` -- which
scripts call constantly -- does not start paying for a measurement it
never uses."
```

---

### Task 2: Daemon sampler worker

**Files:**
- Modify: `crates/asd-daemon/Cargo.toml`
- Create: `crates/asd-daemon/src/metrics.rs`
- Modify: `crates/asd-daemon/src/lib.rs` (module list, lines 12-19)
- Modify: `crates/asd-daemon/src/registry.rs` (struct fields ~line 30, `Registry::new` ~line 44)
- Modify: `crates/asd-daemon/src/server.rs` (one line in `serve`, beside the existing `spawn_cwd_refresh(Arc::clone(&registry));` call)

**Interfaces:**
- Consumes: `asd_proto::HostSample` (Task 1).
- Produces: `Registry::set_host_metrics(&mut self, sample: HostSample)`, `Registry::host_metrics(&self) -> Option<HostSample>`, and `crate::metrics::spawn(registry: Arc<Mutex<Registry>>)`. `rate_bps` and `is_loopback` stay private to `metrics.rs` — only its own tests call them.

- [ ] **Step 1: Write the failing test**

Create `crates/asd-daemon/src/metrics.rs` containing only its tests for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_rate_divides_by_the_interval_actually_measured() {
        // One second of the asked-for interval.
        assert_eq!(rate_bps(1024, Duration::from_secs(1)), 1024);
        // A late worker measured two seconds, so the same delta is half the
        // rate. Dividing by the interval we asked for would report 1024 and
        // overstate the link every time the thread is descheduled.
        assert_eq!(rate_bps(1024, Duration::from_secs(2)), 512);
        // Sub-second intervals scale up, and round rather than truncate.
        assert_eq!(rate_bps(1024, Duration::from_millis(500)), 2048);
    }

    #[test]
    fn a_zero_interval_reports_no_rate_instead_of_dividing_by_zero() {
        assert_eq!(rate_bps(4096, Duration::ZERO), 0);
    }

    #[test]
    fn loopback_interfaces_are_excluded_from_the_sum() {
        assert!(is_loopback("lo"));
        assert!(is_loopback("lo0"));
        assert!(is_loopback("Loopback Pseudo-Interface 1"));
        assert!(!is_loopback("eth0"));
        assert!(!is_loopback("docker0"));
        // Guard against a prefix match that would swallow a real interface
        // whose name merely starts with the same letters.
        assert!(!is_loopback("lodge0"));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --offline -p asd-daemon metrics`
Expected: FAIL — `metrics.rs` is not a module yet, and `rate_bps`/`is_loopback` do not exist.

- [ ] **Step 3: Write the implementation**

Add the dependency:

```bash
cargo add --offline --package asd-daemon sysinfo --no-default-features --features system,network
```

That writes `sysinfo = { version = "0.38.4", default-features = false, features = ["system", "network"] }`. The default features also pull `disk`, `component` and `user`, none of which this uses.

Register the module in `crates/asd-daemon/src/lib.rs`, keeping the list alphabetical:

```rust
mod config;
mod conn;
mod detect;
mod metrics;
mod platform;
mod registry;
mod server;
mod session;
mod store;
```

Put this above the `#[cfg(test)] mod tests` block in `crates/asd-daemon/src/metrics.rs`:

```rust
//! Host resource sampling for the bottom bar.
//!
//! The sampling cadence belongs to the worker here and to nothing else. A
//! client asking for the reading gets whatever the worker stored last, so no
//! number of clients and no fast-polling script can make the daemon measure
//! more often. That is structural, and stronger than a cache with a TTL.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asd_proto::HostSample;
use sysinfo::{Networks, System};

use crate::registry::Registry;

/// How often the worker wakes, and therefore the window every rate is measured
/// over. Must stay above `sysinfo::MINIMUM_CPU_UPDATE_INTERVAL` (~200 ms) or
/// the CPU reading is meaningless.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Bytes per second from a counter delta over the interval it was measured
/// across. Divides by the interval that actually elapsed rather than the one
/// the timer asked for: a descheduled worker measures a longer window, and
/// assuming the shorter one would overstate the link.
fn rate_bps(delta_bytes: u64, elapsed: Duration) -> u64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return 0;
    }
    (delta_bytes as f64 / secs).round() as u64
}

/// Loopback carries a session's own traffic back to itself. Counting it would
/// make a busy local daemon look like a saturated link.
fn is_loopback(interface: &str) -> bool {
    let name = interface.to_ascii_lowercase();
    name == "lo" || name == "lo0" || name.starts_with("loopback")
}

/// Start the sampler. Mirrors `spawn_cwd_refresh` in `server.rs`: a daemon-wide
/// task on a fixed interval, reaching the rest of the daemon through the
/// registry that already flows to every connection.
pub(crate) fn spawn(registry: Arc<Mutex<Registry>>) {
    tokio::spawn(async move {
        let mut system = System::new();
        let mut networks = Networks::new_with_refreshed_list();
        let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately and its rates would divide a full
        // counter by a near-zero interval, so let it prime the deltas only.
        ticker.tick().await;
        let mut last = Instant::now();
        loop {
            ticker.tick().await;
            let elapsed = last.elapsed();
            last = Instant::now();

            // Sample outside the registry lock. sysinfo walks /proc, and the
            // session operations behind that mutex must not wait on it.
            system.refresh_cpu_usage();
            system.refresh_memory();
            networks.refresh(true);
            let (rx, tx) = networks
                .list()
                .iter()
                .filter(|(name, _)| !is_loopback(name))
                .fold((0u64, 0u64), |(rx, tx), (_, data)| {
                    (rx + data.received(), tx + data.transmitted())
                });

            let sample = HostSample {
                // Rounded and clamped here, at the one place that sees the raw
                // reading. Cores are summed and averaged, which can land a
                // hair over 100.
                cpu_pct: system.global_cpu_usage().round().clamp(0.0, 100.0) as u8,
                mem_used_bytes: system.used_memory(),
                mem_total_bytes: system.total_memory(),
                net_rx_bps: rate_bps(rx, elapsed),
                net_tx_bps: rate_bps(tx, elapsed),
                // Filled in when read, not when stored: what a client cares
                // about is how stale the reading is on arrival.
                sampled_age_ms: 0,
            };
            registry.lock().unwrap().set_host_metrics(sample);
        }
    });
}
```

In `crates/asd-daemon/src/registry.rs`, add the field to the `Registry` struct, after `last_persisted`:

```rust
    /// The sampler's most recent reading and when it was taken. `None` until
    /// its first tick.
    host_metrics: Option<(asd_proto::HostSample, std::time::Instant)>,
```

Initialise it in `Registry::new`, after `last_persisted: Vec::new(),`:

```rust
            host_metrics: None,
```

And add the accessors in the same `impl Registry` block:

```rust
    /// Store a fresh reading from the sampler.
    pub fn set_host_metrics(&mut self, sample: asd_proto::HostSample) {
        self.host_metrics = Some((sample, std::time::Instant::now()));
    }

    /// The latest reading with its age filled in. The age is computed here, at
    /// read time, so it measures how stale the reading is when it reaches a
    /// client rather than when it was stored.
    pub fn host_metrics(&self) -> Option<asd_proto::HostSample> {
        self.host_metrics.map(|(sample, at)| asd_proto::HostSample {
            sampled_age_ms: u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX),
            ..sample
        })
    }
```

In `crates/asd-daemon/src/server.rs`, start the worker next to the existing one. Find `spawn_cwd_refresh(Arc::clone(&registry));` in `serve` and add below it:

```rust
    crate::metrics::spawn(Arc::clone(&registry));
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --offline -p asd-daemon metrics`
Expected: PASS — three tests.

- [ ] **Step 5: Check both platforms build**

Run: `cargo check --offline -p asd-daemon`
Run: `cargo check --offline -p asd-daemon --target x86_64-pc-windows-gnu`
Expected: both finish without error. The Windows build can only be checked, not run; see `docs/cross-platform-development.md`.

- [ ] **Step 6: Commit**

```bash
git add crates/asd-daemon/Cargo.toml Cargo.lock crates/asd-daemon/src/metrics.rs crates/asd-daemon/src/lib.rs crates/asd-daemon/src/registry.rs crates/asd-daemon/src/server.rs
git commit -m "feat(daemon): sample host CPU, memory and network on a worker

The sampling cadence belongs to the worker and to nothing else, so a
client asking for the reading cannot make the daemon measure more often --
which a measure-on-request design would have allowed any polling script to
do. Rates divide by the interval actually measured, because a descheduled
worker covers a longer window than the timer asked for."
```

---

### Task 3: Daemon answers the frame

**Files:**
- Modify: `crates/asd-daemon/src/conn.rs` (add an arm beside `Frame::ListSessions`, ~line 124)
- Modify: `tests/e2e.rs` (add one test)

**Interfaces:**
- Consumes: `Registry::host_metrics()` (Task 2), `Frame::HostMetrics` / `Frame::HostMetricsReply` (Task 1).
- Produces: a daemon that answers `HostMetrics`.

- [ ] **Step 1: Write the failing test**

Append to `tests/e2e.rs`, using the helpers that file already has: `Daemon::start(tag)` gives a throwaway daemon with a `.socket`, and `ProtoClient::connect(&daemon.socket)` gives a client with `send`/`recv`.

```rust
/// The daemon answers a metrics request out of its sampler's stored reading.
/// `None` is a real answer, not a failure -- the sampler primes for a second
/// after start-up. What must not happen is an error, a wrong frame, or a hang.
#[tokio::test]
async fn host_metrics_are_served_from_the_daemon() {
    let daemon = Daemon::start("host-metrics");
    let mut c = ProtoClient::connect(&daemon.socket).await;

    c.send(Frame::HostMetrics).await;
    match c.recv().await {
        Frame::HostMetricsReply { sample: None } => {}
        Frame::HostMetricsReply { sample: Some(s) } => {
            // u8 is unsigned, so only the upper bound is worth asserting.
            assert!(s.cpu_pct <= 100, "cpu out of range: {}", s.cpu_pct);
            assert!(s.mem_total_bytes > 0, "a host with no memory is not real");
            assert!(s.mem_used_bytes <= s.mem_total_bytes);
        }
        other => panic!("expected HostMetricsReply, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --offline --test e2e host_metrics_are_served_from_the_daemon`
Expected: FAIL — the daemon does not handle `Frame::HostMetrics`, so the test panics on an unexpected frame or times out.

- [ ] **Step 3: Write the implementation**

In `crates/asd-daemon/src/conn.rs`, add this arm directly after the `Frame::ListSessions` arm:

```rust
            Frame::HostMetrics => {
                // Read the sampler's stored reading. Nothing is measured here:
                // see `crate::metrics`.
                reply(Frame::HostMetricsReply {
                    sample: registry.lock().unwrap().host_metrics(),
                });
            }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --offline --test e2e host_metrics_are_served_from_the_daemon`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/asd-daemon/src/conn.rs tests/e2e.rs
git commit -m "feat(daemon): answer HostMetrics from the stored sample"
```

---

### Task 4: TUI transport — ask for it and carry it into `App`

**Files:**
- Modify: `crates/asd-tui/src/conn.rs` (`Ev` enum line 53, ticker arm ~line 176, frame match ~line 182)
- Modify: `crates/asd-tui/src/lib.rs` (`App` struct ~line 481, event handling near `Ev::Sessions` line 1232)

**Interfaces:**
- Consumes: `Frame::HostMetrics`, `Frame::HostMetricsReply`, `asd_proto::HostSample` (Task 1).
- Produces: `Ev::Metrics(Option<asd_proto::HostSample>)`, and `App.metrics: Option<asd_proto::HostSample>` for Task 6 to render.

- [ ] **Step 1: Add the event variant**

In `crates/asd-tui/src/conn.rs`, add to the `Ev` enum:

```rust
    /// The daemon host's latest resource reading, or `None` while its sampler
    /// is still priming.
    Metrics(Option<asd_proto::HostSample>),
```

- [ ] **Step 2: Ask for it on the existing ticker**

Still in `conn.rs`, in the `_ = ticker.tick()` arm, after the `ListSessions` write:

```rust
                if writer.write_frame(&Frame::HostMetrics).await.is_err() {
                    return Err("metrics write failed".to_string());
                }
```

The ticker runs at `LIST_INTERVAL` (1500 ms) against a 1000 ms sampler, so the bar never shows a reading much more than a second old.

- [ ] **Step 3: Handle the reply**

Add a match arm beside `Ok(Some(Frame::SessionList { .. }))`:

```rust
                Ok(Some(Frame::HostMetricsReply { sample })) => {
                    let _ = ev_tx.send(Ev::Metrics(sample));
                }
```

- [ ] **Step 4: Store it on `App`**

In `crates/asd-tui/src/lib.rs`, add the field to `App` next to `now_ms`:

```rust
    /// The daemon host's latest resource reading, for the bottom bar. `None`
    /// before the first reply arrives.
    pub metrics: Option<asd_proto::HostSample>,
```

Initialise it to `None` wherever `App` is constructed (the same literal that sets `now_ms: now_ms(),`):

```rust
        metrics: None,
```

Handle the event beside `Ev::Sessions`:

```rust
            Ev::Metrics(sample) => {
                self.metrics = sample;
                self.dirty = true;
            }
```

- [ ] **Step 5: Verify it compiles and nothing regressed**

Run: `cargo test --offline -p asd-tui`
Expected: PASS — no behaviour has changed yet; the bar still ignores `App.metrics`.

- [ ] **Step 6: Commit**

```bash
git add crates/asd-tui/src/conn.rs crates/asd-tui/src/lib.rs
git commit -m "feat(tui): carry the host sample from the daemon into App

Rides the ticker that already polls ListSessions rather than adding a
second timer, so the bar's data arrives on the cadence the sidebar
already refreshes on."
```

---

### Task 5: Bar formatting and colour helpers

**Files:**
- Modify: `crates/asd-tui/src/ui/bar.rs`

**Interfaces:**
- Consumes: the palette `OK`, `ACCENT`, `ALERT`, `MUTED`, `DIM`, `RULE` from `super` (already imported for `DIM`, `ACCENT`, `ALERT`, `OK`, `RULE`; add `MUTED`).
- Produces: `fmt_bytes(bytes: u64) -> String`, `fmt_pct(pct: u8) -> String`, `load_color(pct: u8) -> Color`, all private to `bar.rs` and used by Task 6.

- [ ] **Step 1: Write the failing tests**

Add to the existing `mod tests` in `crates/asd-tui/src/ui/bar.rs`:

```rust
    #[test]
    fn bytes_read_the_way_free_h_writes_them() {
        // One decimal below ten, none above, so a value never jitters between
        // widths as it crosses a round number.
        assert_eq!(fmt_bytes(6_549_123_456), "6.1G");
        assert_eq!(fmt_bytes(33_285_996_544), "31G");
        assert_eq!(fmt_bytes(1_258_291), "1.2M");
        assert_eq!(fmt_bytes(348_160), "340K");
        // Below a kibibyte there is no unit worth scaling to.
        assert_eq!(fmt_bytes(512), "512B");
        assert_eq!(fmt_bytes(0), "0B");
        assert_eq!(fmt_bytes(1023), "1023B");
    }

    #[test]
    fn a_size_that_rounds_up_promotes_instead_of_reading_1024() {
        // One byte under each boundary. Choosing the unit from the raw ratio
        // and rounding afterwards renders these "1024K" and "1024M", which is
        // not a unit anyone writes.
        assert_eq!(fmt_bytes((1 << 20) - 1), "1.0M");
        assert_eq!(fmt_bytes((1 << 30) - 1), "1.0G");
        // Exactly on the boundary, for the other side of the same fence.
        assert_eq!(fmt_bytes(1 << 20), "1.0M");
        assert_eq!(fmt_bytes(1 << 30), "1.0G");
    }

    #[test]
    fn a_size_that_rounds_up_to_ten_drops_its_decimal() {
        // 9.95 GiB and up round to ten. Keeping the decimal there widens the
        // field from "9.9G" to "10.0G", which is the jitter the one-decimal
        // rule exists to avoid.
        assert_eq!(fmt_bytes(10_684_795_973), "10G");
        assert_eq!(fmt_bytes(10_630_000_000), "9.9G");
    }

    #[test]
    fn a_percent_is_printed_whole_and_never_over_a_hundred() {
        assert_eq!(fmt_pct(0), "0%");
        assert_eq!(fmt_pct(12), "12%");
        assert_eq!(fmt_pct(100), "100%");
        // The sampler clamps, but a value that got past it should read as a
        // busy host rather than a number that looks like a bug.
        assert_eq!(fmt_pct(103), "100%");
    }

    #[test]
    fn load_colour_escalates_at_the_documented_thresholds() {
        assert_eq!(load_color(0), OK);
        assert_eq!(load_color(69), OK);
        assert_eq!(load_color(70), ACCENT);
        assert_eq!(load_color(89), ACCENT);
        assert_eq!(load_color(90), ALERT);
        assert_eq!(load_color(100), ALERT);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --offline -p asd-tui bar`
Expected: FAIL — `fmt_bytes`, `fmt_pct` and `load_color` do not exist.

- [ ] **Step 3: Write the implementation**

Add `MUTED` to the `use super::{...}` line at the top of `bar.rs`, and `ratatui::style::Color` to the imports. Then add:

```rust
/// Binary units the way `free -h` writes them: one decimal below ten, none at
/// or above it. The bar is short on columns, so nothing is padded.
///
/// The unit is chosen from the value as it will be *rendered*, not from the
/// raw ratio. Picking the unit first and rounding afterwards is what produces
/// "1024K" for one byte under a mebibyte, and "10.0G" — a decimal at ten — for
/// anything from 9.95 GiB up.
fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [char; 3] = ['K', 'M', 'G'];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    // Promote while the whole-number form would read 1024 or more.
    while unit + 1 < UNITS.len() && value >= 1023.5 {
        value /= 1024.0;
        unit += 1;
    }
    let suffix = UNITS[unit];
    if value < 9.95 {
        format!("{value:.1}{suffix}")
    } else {
        format!("{}{suffix}", value.round() as u64)
    }
}

/// A whole percent, clamped. The sampler already clamps; this is the second
/// belt, because printing "103%" reads like a bug rather than a busy host.
fn fmt_pct(pct: u8) -> String {
    format!("{}%", pct.min(100))
}

/// Green until the host is working, amber while it is, red once it is out of
/// room. Only CPU and memory get this: there is no throughput that is "bad",
/// so colouring the network would raise an alarm that means nothing.
fn load_color(pct: u8) -> Color {
    if pct >= 90 {
        ALERT
    } else if pct >= 70 {
        ACCENT
    } else {
        OK
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --offline -p asd-tui bar`
Expected: PASS — the three new tests plus the two existing bar tests, which are untouched so far.

- [ ] **Step 5: Commit**

```bash
git add crates/asd-tui/src/ui/bar.rs
git commit -m "feat(tui): formatting and colour helpers for the bar's metrics"
```

---

### Task 6: Render the segments, with the drop order

**Files:**
- Modify: `crates/asd-tui/src/ui/bar.rs` (`draw`, `draw_text`, `draw_left`, and the two existing tests)

**Interfaces:**
- Consumes: `App.metrics` (Task 4), `fmt_bytes` / `fmt_pct` / `load_color` (Task 5).
- Produces: the finished bar. Nothing else depends on it.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `bar.rs`:

```rust
    fn sample() -> asd_proto::HostSample {
        asd_proto::HostSample {
            cpu_pct: 12,
            mem_used_bytes: 6_549_123_456,
            mem_total_bytes: 33_285_996_544,
            net_rx_bps: 1_258_291,
            net_tx_bps: 348_160,
            sampled_age_ms: 740,
        }
    }

    fn rendered(width: u16, metrics: Option<asd_proto::HostSample>) -> String {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        draw_text(
            &mut buf,
            area,
            &Keymap::default().current_hint(),
            "2026-08-14 09:05:07",
            "● 3 sessions",
            Style::default(),
            metrics,
        );
        (0..area.width)
            .map(|x| buf.cell(Position::new(x, 0)).unwrap().symbol())
            .collect()
    }

    #[test]
    fn a_wide_bar_shows_every_segment() {
        let line = rendered(160, Some(sample()));
        assert!(line.contains("Keybinds: Ctrl+A"), "bar: {line}");
        assert!(line.contains("2026-08-14 09:05:07"), "bar: {line}");
        assert!(line.contains("12%"), "bar: {line}");
        assert!(line.contains("6.1G/31G"), "bar: {line}");
        assert!(line.contains("↓1.2M ↑340K"), "bar: {line}");
        assert!(line.contains("● 3 sessions"), "bar: {line}");
    }

    #[test]
    fn segments_drop_right_to_left_as_the_bar_narrows() {
        // Network goes first, then memory, then CPU, then the clock. The
        // keybind hint outlives them all because it is the only actionable
        // thing on the bar, and the clock outlives the new segments so a narrow
        // terminal keeps behaving the way it did before they existed.
        let mut seen_without = Vec::new();
        for width in [160u16, 120, 100, 84, 64, 42] {
            let line = rendered(width, Some(sample()));
            seen_without.push((
                width,
                line.contains("↓1.2M"),
                line.contains("6.1G/31G"),
                line.contains("12%"),
                line.contains("2026-08-14"),
                line.contains("Keybinds"),
            ));
        }
        // Whatever the exact widths, a segment never comes back once dropped,
        // and the keybind hint is present at every width.
        let mut net = true;
        let mut mem = true;
        let mut cpu = true;
        let mut clock = true;
        for (width, has_net, has_mem, has_cpu, has_clock, has_keys) in seen_without {
            assert!(has_keys, "keybinds vanished at {width}");
            assert!(!has_net || net, "network came back at {width}");
            assert!(!has_mem || mem, "memory came back at {width}");
            assert!(!has_cpu || cpu, "cpu came back at {width}");
            assert!(!has_clock || clock, "clock came back at {width}");
            // Ordering: a segment cannot outlive one to its right.
            assert!(has_net <= has_mem, "memory dropped before network at {width}");
            assert!(has_mem <= has_cpu, "cpu dropped before memory at {width}");
            assert!(has_cpu <= has_clock, "clock dropped before cpu at {width}");
            net = has_net;
            mem = has_mem;
            cpu = has_cpu;
            clock = has_clock;
        }
    }

    #[test]
    fn without_a_sample_the_bar_looks_exactly_as_it_did_before() {
        let line = rendered(160, None);
        assert!(
            line.starts_with(" Keybinds: Ctrl+A  2026-08-14 09:05:07"),
            "bar: {line}"
        );
        assert!(!line.contains('%'), "bar: {line}");
    }
```

Then update the two existing tests, which call `draw_text` with the old six-argument signature: add `None` as the new last argument to both `bottom_bar_places_the_full_server_time_after_keybinds` and `narrow_bottom_bar_keeps_daemon_status_before_the_clock`. Their assertions are unchanged — with no sample, the bar must render exactly as it does today.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --offline -p asd-tui bar`
Expected: FAIL — `draw_text` takes six arguments, not seven.

- [ ] **Step 3: Write the implementation**

Replace `draw`, `draw_text` and `draw_left` in `bar.rs` with:

```rust
pub(super) fn draw(buf: &mut Buffer, area: Rect, app: &App) {
    if area.height == 0 || area.width < 8 {
        return;
    }
    for x in area.left()..area.right() {
        if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(x, area.top())) {
            cell.set_style(Style::new().bg(RULE));
        }
    }
    let (status, status_style) = status(app);
    draw_text(
        buf,
        area,
        &app.keymap.current_hint(),
        &server_time_at(app.now_ms),
        &status,
        status_style,
        app.metrics,
    );
}

/// One run of the left-hand group: a dim icon and a coloured value. The icon
/// carries no colour of its own so the eye lands on the number.
struct Segment {
    icon: &'static str,
    value: String,
    value_style: Style,
}

impl Segment {
    fn width(&self) -> usize {
        // icon + one space + value
        str_width(self.icon) + 1 + str_width(&self.value)
    }
}

/// The metric segments in display order. Dropping from the end of this vector
/// is what produces the documented narrow-terminal order: network, then
/// memory, then CPU, then the clock.
fn segments(server_time: &str, metrics: Option<asd_proto::HostSample>) -> Vec<Segment> {
    let mut out = vec![Segment {
        icon: "🕐",
        value: server_time.to_string(),
        value_style: Style::new().fg(MUTED).bg(RULE),
    }];
    let Some(m) = metrics else {
        return out;
    };
    out.push(Segment {
        icon: "💻",
        value: fmt_pct(m.cpu_pct),
        value_style: Style::new().fg(load_color(m.cpu_pct)).bg(RULE),
    });
    let mem_pct = if m.mem_total_bytes == 0 {
        0
    } else {
        ((m.mem_used_bytes as f64 / m.mem_total_bytes as f64) * 100.0).round() as u8
    };
    out.push(Segment {
        icon: "🧠",
        value: format!(
            "{}/{}",
            fmt_bytes(m.mem_used_bytes),
            fmt_bytes(m.mem_total_bytes)
        ),
        value_style: Style::new().fg(load_color(mem_pct)).bg(RULE),
    });
    out.push(Segment {
        icon: "🌐",
        value: format!("↓{} ↑{}", fmt_bytes(m.net_rx_bps), fmt_bytes(m.net_tx_bps)),
        value_style: Style::new().fg(MUTED).bg(RULE),
    });
    out
}

fn draw_text(
    buf: &mut Buffer,
    area: Rect,
    hint: &KeyHint,
    server_time: &str,
    status: &str,
    status_style: Style,
    metrics: Option<asd_proto::HostSample>,
) {
    let status = truncate(status, (area.width / 2) as usize);
    let x = area.right().saturating_sub(str_width(&status) as u16 + 1);
    buf.set_string(x, area.top(), status, status_style);
    let left_width = x.saturating_sub(area.left() + 2) as usize;
    draw_left(buf, area, hint, server_time, metrics, left_width);
}

fn draw_left(
    buf: &mut Buffer,
    area: Rect,
    hint: &KeyHint,
    server_time: &str,
    metrics: Option<asd_proto::HostSample>,
    max_width: usize,
) {
    let hint_style = if hint.prefix_active {
        Style::new().fg(ACCENT).bg(RULE)
    } else {
        Style::new().fg(DIM).bg(RULE)
    };
    let icon_style = Style::new().fg(DIM).bg(RULE);

    // Two spaces separate the keybind hint from the first segment and each
    // segment from the next, matching the gap the clock has always had.
    const GAP: usize = 2;
    let mut segs = segments(server_time, metrics);
    let hint_width = str_width(&hint.text);
    while !segs.is_empty()
        && hint_width + segs.iter().map(|s| GAP + s.width()).sum::<usize>() > max_width
    {
        segs.pop();
    }

    let mut x = area.left() + 1;
    if segs.is_empty() {
        // Nothing else fits: the hint alone, truncated, as it has always been.
        buf.set_string(x, area.top(), truncate(&hint.text, max_width), hint_style);
        return;
    }
    buf.set_string(x, area.top(), &hint.text, hint_style);
    x += hint_width as u16;
    for seg in &segs {
        x += GAP as u16;
        buf.set_string(x, area.top(), seg.icon, icon_style);
        x += str_width(seg.icon) as u16 + 1;
        buf.set_string(x, area.top(), &seg.value, seg.value_style);
        x += str_width(&seg.value) as u16;
    }
}
```

Note the behaviour this preserves: with `metrics == None` the segment list holds only the clock, so the bar renders exactly as it does today, and the existing tests still pass unchanged.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --offline -p asd-tui`
Expected: PASS — new bar tests plus the two updated ones.

- [ ] **Step 5: Commit**

```bash
git add crates/asd-tui/src/ui/bar.rs
git commit -m "feat(tui): show host CPU, memory and bandwidth in the bottom bar

Segments drop right to left as the terminal narrows -- network, memory,
CPU, then the clock -- so the keybind hint, the only actionable thing on
the bar, is the last to go. With no sample the bar renders exactly as it
did before, which is what the untouched assertions in the two older tests
are there to hold."
```

---

### Task 7: Whole-workspace verification and the deployment note

**Files:**
- Modify: `docs/superpowers/specs/2026-08-20-tui-host-metrics-bar-design.md` (status line only)

**Interfaces:**
- Consumes: everything above.
- Produces: nothing further depends on this.

- [ ] **Step 1: Run every required check**

```bash
cargo test --offline --workspace
cargo clippy --offline --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
cargo test --offline -p asd-proto
cargo check --offline --workspace --target x86_64-pc-windows-gnu
```

Expected: all clean. Fix anything that is not before continuing.

- [ ] **Step 2: See it running**

```bash
cargo build --offline --release
```

Then start an isolated daemon and TUI rather than touching the user's — the machine this was written on runs long-lived agents inside `asd` sessions, and restarting their daemon kills all of them:

```bash
mkdir -p /tmp/asdbar/share
XDG_DATA_HOME=/tmp/asdbar/share ASD_SOCKET=/tmp/asdbar/d.sock ./target/release/asd new bartest
XDG_DATA_HOME=/tmp/asdbar/share ASD_SOCKET=/tmp/asdbar/d.sock ./target/release/asd ui
```

Confirm by eye: four icons, values that move, colours that differ, and a narrow window that drops segments in the documented order. Then clean up by killing that daemon's PID specifically — never `pkill -f asd`, which on this machine also matches the user's real daemon and the shell running the command.

- [ ] **Step 3: Mark the spec implemented**

Change the spec's `Status:` line to `implemented`.

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-08-20-tui-host-metrics-bar-design.md
git commit -m "docs: mark the host-metrics bar spec implemented"
```

- [ ] **Step 5: Tell the user what deploying costs**

Do not deploy or restart anything. Report instead:

`PROTO_VERSION` moved 14 → 15, and the protocol has no cross-version compatibility, so the new binary and the running daemon cannot talk to each other. Every client — `asd ui`, `asd list`, the GUI — starts failing against the old daemon the moment the binary is replaced, and only works again once the daemon restarts. Restarting it kills every session's child process. On the machine this was written on that is 21 sessions, 19 of them running agents, several mid-task, including the session the assistant itself runs in. The user picks the moment.
