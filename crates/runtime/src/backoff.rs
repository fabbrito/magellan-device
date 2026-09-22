//! The ladder a device climbs while the cloud refuses it.
//!
//! Two climbers: the manifest before anything is sent, the buffer after. Written out separately
//! once, and they drifted — a spread on one and not the other is a fleet retrying in step.

use std::time::Duration;

/// One device's place on the backoff ladder.
///
/// A wait is a spread of the rung, never the rung itself: the ladder bounds a device and does
/// not schedule a fleet.
pub(crate) struct Backoff {
    rung: Duration,
    first: Duration,
    ceiling: Duration,
    jitter: Jitter,
}

impl Backoff {
    /// A ladder from `first` to `ceiling`, spread apart from every other device's.
    pub(crate) fn new(first: Duration, ceiling: Duration) -> Self {
        Self::seeded(first, ceiling, platform::random_seed())
    }

    /// The same ladder from a named seed, so a test reads one device's sequence.
    pub(crate) fn seeded(first: Duration, ceiling: Duration, seed: u64) -> Self {
        // A cadence built wrong, not an outage: the ladder would descend on its first climb.
        assert!(first <= ceiling, "a backoff ceiling under its first rung");
        Self {
            rung: first,
            first,
            ceiling,
            jitter: Jitter::seeded(seed),
        }
    }

    /// The wait to take now, after which the next refusal starts a rung higher.
    pub(crate) fn climb(&mut self) -> Duration {
        let wait = self.jitter.spread(self.rung);
        self.rung = self.rung.saturating_mul(2).min(self.ceiling);
        wait
    }

    /// Back to the first rung: the cloud answered.
    pub(crate) fn reset(&mut self) {
        self.rung = self.first;
    }
}

/// Spreads a wait so two devices that failed together do not come back together.
///
/// `SplitMix64`, seeded once per run: enough to decorrelate a fleet and nothing more — no
/// cryptographic claim, and no dependency for one.
struct Jitter(u64);

impl Jitter {
    /// A sequence of its own, from a seed the platform drew.
    const fn seeded(seed: u64) -> Self {
        Self(seed)
    }

    /// Half of `wait`, plus up to half again: the rung keeps its floor.
    fn spread(&mut self, wait: Duration) -> Duration {
        let millis = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
        let half = millis / 2;
        // A rung under two milliseconds has no room to spread; the maximum keeps `%` defined.
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

    /// Rungs told apart at a glance: 8ms, doubling to a 32ms ceiling.
    fn ladder() -> Backoff {
        Backoff::seeded(Duration::from_millis(8), Duration::from_millis(32), 1)
    }

    #[test]
    fn a_climb_doubles_the_rung_and_stops_at_the_ceiling() {
        let mut backoff = ladder();
        let mut rungs = Vec::new();
        for _ in 0..5 {
            rungs.push(backoff.rung);
            backoff.climb();
        }
        assert_eq!(rungs, [8, 16, 32, 32, 32].map(Duration::from_millis));
    }

    #[test]
    fn a_wait_keeps_its_rung_as_a_floor_and_a_ceiling() {
        // The spread moves where inside the rung a device lands, never whether it waited.
        let mut backoff = Backoff::seeded(Duration::from_secs(300), Duration::from_secs(300), 1);
        let mut landed = std::collections::BTreeSet::new();
        for _ in 0..1_000 {
            let wait = backoff.climb();
            assert!(
                wait >= Duration::from_secs(150),
                "{wait:?} is under the rung"
            );
            assert!(wait < Duration::from_secs(300), "{wait:?} is past the rung");
            landed.insert(wait);
        }
        assert!(
            landed.len() > 100,
            "barely spread: {} instants",
            landed.len()
        );
    }

    #[test]
    fn an_answer_returns_the_ladder_to_its_first_rung() {
        let mut backoff = ladder();
        for _ in 0..4 {
            backoff.climb();
        }
        assert_eq!(backoff.rung, Duration::from_millis(32));
        backoff.reset();
        assert_eq!(backoff.rung, Duration::from_millis(8), "the ladder held");
    }

    #[test]
    fn two_devices_do_not_come_back_together() {
        // The herd this exists to break up: one outage ends for the whole fleet at once.
        let rung = Duration::from_secs(300);
        let (mut one, mut other) = (Jitter::seeded(1), Jitter::seeded(2));
        let together = (0..16)
            .filter(|_| one.spread(rung) == other.spread(rung))
            .count();
        assert!(together <= 1, "retried in step {together} times in 16");
    }

    #[test]
    fn a_rung_too_short_to_spread_is_waited_rather_than_divided_by_zero() {
        let mut jitter = Jitter::seeded(1);
        assert_eq!(jitter.spread(Duration::ZERO), Duration::ZERO);
        assert_eq!(jitter.spread(Duration::from_millis(1)), Duration::ZERO);
    }
}
