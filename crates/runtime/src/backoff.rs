//! How long a device waits while the cloud refuses it.
//!
//! Two users: the manifest before anything is sent, the buffer after. Written out separately once,
//! and they drifted — a spread on one and not the other is a fleet retrying in step.

use std::time::Duration;

/// One device's backoff: an interval doubling to a ceiling.
///
/// A wait is a spread of the interval, never the interval itself: it bounds a device and does not
/// schedule a fleet.
pub(crate) struct Backoff {
    interval: Duration,
    first: Duration,
    ceiling: Duration,
    jitter: Jitter,
}

impl Backoff {
    /// A backoff from `first` to `ceiling`, spread by `seed`.
    pub(crate) fn seeded(first: Duration, ceiling: Duration, seed: u64) -> Self {
        // A cadence built wrong, not an outage: the interval would shrink on its first doubling.
        assert!(
            first <= ceiling,
            "a backoff ceiling under its first interval"
        );
        Self {
            interval: first,
            first,
            ceiling,
            jitter: Jitter::seeded(seed),
        }
    }

    /// The wait to take now; the next refusal waits on a doubled interval.
    pub(crate) fn next_wait(&mut self) -> Duration {
        let wait = self.jitter.spread(self.interval);
        self.interval = self.interval.saturating_mul(2).min(self.ceiling);
        wait
    }

    /// Back to the first interval: the cloud answered.
    pub(crate) fn reset(&mut self) {
        self.interval = self.first;
    }
}

/// A spread seed from the boot id, which already carries the entropy one needs: a second read of
/// the kernel's source would need a fallback, and a fallback goes quiet.
#[must_use]
pub fn jitter_seed(boot_id: &str) -> u64 {
    // FNV-1a: any length, infallible, and every byte moves the seed.
    boot_id.bytes().fold(0xCBF2_9CE4_8422_2325, |seed, byte| {
        (seed ^ u64::from(byte)).wrapping_mul(0x0100_0000_01B3)
    })
}

/// Spreads a wait so two devices that failed together do not come back together.
///
/// `SplitMix64`, seeded once per run: enough to decorrelate a fleet and nothing more — no
/// cryptographic claim, and no dependency for one.
struct Jitter(u64);

impl Jitter {
    /// A sequence of its own, from the device's seed.
    const fn seeded(seed: u64) -> Self {
        Self(seed)
    }

    /// Half of `interval`, plus up to half again.
    fn spread(&mut self, interval: Duration) -> Duration {
        let millis = u64::try_from(interval.as_millis()).unwrap_or(u64::MAX);
        let half = millis / 2;
        // Under two milliseconds there is no room to spread; the maximum keeps `%` defined.
        Duration::from_millis(half.saturating_add(self.draw() % half.max(1)))
    }

    /// `SplitMix64`'s mixer, constants and all.
    fn draw(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut drawn = self.0;
        drawn = (drawn ^ (drawn >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        drawn = (drawn ^ (drawn >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        drawn ^ (drawn >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Intervals told apart at a glance: 8ms, doubling to a 32ms ceiling.
    fn backoff() -> Backoff {
        Backoff::seeded(Duration::from_millis(8), Duration::from_millis(32), 1)
    }

    #[test]
    fn the_interval_doubles_and_stops_at_the_ceiling() {
        let mut backoff = backoff();
        let mut intervals = Vec::new();
        for _ in 0..5 {
            intervals.push(backoff.interval);
            backoff.next_wait();
        }
        assert_eq!(intervals, [8, 16, 32, 32, 32].map(Duration::from_millis));
    }

    #[test]
    fn a_wait_stays_inside_the_interval_it_came_from() {
        // The spread moves where inside the interval a device lands, never whether it waited.
        let mut backoff = Backoff::seeded(Duration::from_secs(300), Duration::from_secs(300), 1);
        let mut landed = std::collections::BTreeSet::new();
        for _ in 0..1_000 {
            let wait = backoff.next_wait();
            assert!(wait >= Duration::from_secs(150), "{wait:?} under half");
            assert!(
                wait < Duration::from_secs(300),
                "{wait:?} past the interval"
            );
            landed.insert(wait);
        }
        assert!(
            landed.len() > 100,
            "barely spread: {} instants",
            landed.len()
        );
    }

    #[test]
    fn an_answer_returns_the_backoff_to_its_first_interval() {
        let mut backoff = backoff();
        for _ in 0..4 {
            backoff.next_wait();
        }
        assert_eq!(backoff.interval, Duration::from_millis(32));
        backoff.reset();
        assert_eq!(backoff.interval, Duration::from_millis(8), "did not reset");
    }

    #[test]
    fn two_devices_do_not_come_back_together() {
        // The herd this exists to break up: one outage ends for the whole fleet at once.
        let interval = Duration::from_secs(300);
        let (mut one, mut other) = (Jitter::seeded(1), Jitter::seeded(2));
        let together = (0..16)
            .filter(|_| one.spread(interval) == other.spread(interval))
            .count();
        assert!(together <= 1, "retried in step {together} times in 16");
    }

    #[test]
    fn two_boot_ids_seed_two_spreads() {
        assert_ne!(
            jitter_seed("0123456789abcdef"),
            jitter_seed("0123456789abcdee")
        );
    }

    #[test]
    fn an_interval_too_short_to_spread_is_waited_rather_than_divided_by_zero() {
        let mut jitter = Jitter::seeded(1);
        assert_eq!(jitter.spread(Duration::ZERO), Duration::ZERO);
        assert_eq!(jitter.spread(Duration::from_millis(1)), Duration::ZERO);
    }
}
