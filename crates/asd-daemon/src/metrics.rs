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
