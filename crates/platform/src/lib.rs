//! Layer 7 — the platform seam. The runtime never names an OS; a platform supplies the clock, the
//! sleep and the flash the buffer spills to. One platform is built — Linux on 32-bit ARM — and the
//! seam stays anyway, because the runtime is written against a shape rather than against an OS.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Wall-clock and monotonic time, in the units the contract and the heartbeat use.
pub trait Clock {
    /// Milliseconds since the Unix epoch, UTC — the `Reading::ts` the runtime stamps.
    fn now_ms(&self) -> u64;

    /// Seconds since boot. The heartbeat's `boot_id` is what makes a reset explainable, not uptime.
    fn uptime_seconds(&self) -> u64;
}

/// The Linux clock: wall time from the OS, uptime from a monotonic instant taken at construction.
///
/// Uptime is therefore since this process started, not since the board powered on. For a device
/// whose binary starts at boot they are the same number, and where they differ the process restart
/// is the event worth seeing — it comes paired with a fresh `boot_id`.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    started: Instant,
}

impl SystemClock {
    /// Start the uptime count. Call once, where the program starts.
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        // A Pi carries no clock of its own until NTP steps it, so this reads 1970 for the first
        // seconds of a boot. Reported as it is: a timestamp the runtime can see is wrong beats one
        // the platform quietly invented.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            })
    }

    fn uptime_seconds(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2020-01-01T00:00:00Z in milliseconds. A real clock is past it; the same instant expressed
    /// in seconds is not, so this one bound catches both a dead clock and a units slip.
    const A_DATE_ALREADY_PAST: u64 = 1_577_836_800_000;

    #[test]
    fn the_wall_clock_reads_milliseconds_past_a_date_already_gone() {
        assert!(SystemClock::new().now_ms() > A_DATE_ALREADY_PAST);
    }

    #[test]
    fn uptime_starts_at_zero_and_never_goes_back() {
        let clock = SystemClock::new();
        assert_eq!(clock.uptime_seconds(), 0);
        let first = clock.uptime_seconds();
        let second = clock.uptime_seconds();
        assert!(second >= first);
    }
}
