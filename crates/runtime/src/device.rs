//! Assembling what the device sends: the manifest it declares, and the batches it stamps.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use contract::{Batch, Heartbeat, Manifest, Reading};
use driver::Source;
use jiff::Timestamp;
use platform::Clock;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::backoff::Backoff;
use crate::window::{self, Now, Sun};
use crate::{Buffer, Cloud, Outcome, Queue, drain_once};

/// The manifest these sources declare, in the order they are polled.
///
/// Composed rather than written down, so adding a source or a register to a driver's profile
/// changes what the cloud stores with no cloud deploy and nothing edited here.
#[must_use]
pub fn manifest_of(sources: &[Box<dyn Source>]) -> Manifest {
    Manifest {
        sources: sources
            .iter()
            .map(|source| contract::Source {
                id: source.id().to_owned(),
                metrics: source.metrics().to_vec(),
            })
            .collect(),
    }
}

/// Stamps batches for one run of the device.
///
/// The boot id is drawn once and the counter starts at zero, which together identify a batch
/// (ADR 8). The counter advances whether or not a batch is ever delivered — a number spent on a
/// batch the buffer later drops is exactly the gap that makes the loss visible.
#[derive(Debug)]
pub struct Batches {
    manifest_hash: String,
    boot_id: String,
    seq: u64,
}

impl Batches {
    /// Stamp for the manifest the cloud accepted, in the boot `boot_id` names.
    #[must_use]
    pub const fn new(manifest_hash: String, boot_id: String) -> Self {
        Self {
            manifest_hash,
            boot_id,
            seq: 0,
        }
    }

    /// The next batch. Consumes a `seq` even if nothing ever sends it.
    pub fn stamp(&mut self, readings: Vec<Reading>, heartbeat: Heartbeat) -> Batch {
        let batch = Batch {
            manifest_hash: self.manifest_hash.clone(),
            boot_id: self.boot_id.clone(),
            seq: self.seq.to_string(),
            readings,
            heartbeat,
        };
        // `u64` outlasts any device polling every few minutes. Saturating rather than
        // wrapping so the impossible case repeats one number instead of replaying the
        // whole range against a cloud that deduplicates on it.
        self.seq = self.seq.saturating_add(1);
        batch
    }

    /// The hash these batches name.
    #[must_use]
    pub fn manifest_hash(&self) -> &str {
        &self.manifest_hash
    }
}

/// The device's account of itself, as of now.
#[must_use]
pub fn heartbeat(clock: &dyn Clock, buffer_depth: u32, firmware: &str) -> Heartbeat {
    Heartbeat {
        firmware_version: Some(firmware.to_owned()),
        ..Heartbeat::new(clock.uptime_seconds(), buffer_depth)
    }
}

/// How long the device waits between things.
#[derive(Debug, Clone, Copy)]
pub struct Cadence {
    /// One sweep of every source per slot.
    pub sweep: Duration,
    /// First wait after the cloud declines a batch; doubles up to `backoff_max`. Spread before
    /// it is taken, so the rung is a ceiling and never a schedule.
    pub backoff_min: Duration,
    /// Longest the device waits before trying the cloud again.
    pub backoff_max: Duration,
    /// Least time between two sends while the buffer drains: a backlog is already minutes old,
    /// and a cloud that has just come back is owed no burst.
    pub drain_pace: Duration,
    /// Longest a closed window is slept on before looking again. The board has no clock of its
    /// own until the network steps it, so a sleep computed until sunrise at boot can land hours
    /// out; looking again settles it.
    pub recheck: Duration,
}

/// Slots are multiples of `period` since the epoch, so a restart rejoins the same grid rather
/// than starting a new one.
fn until_next_slot(now_ms: u64, period: Duration) -> Duration {
    let period_ms = u64::try_from(period.as_millis()).unwrap_or(u64::MAX).max(1);
    Duration::from_millis(period_ms - now_ms % period_ms)
}

/// One sweep: poll every source and keep what answered.
///
/// A source that times out or refuses leaves its readings out and the sweep goes on. A poll that
/// fails is normal — the buffer carries the gap — so one silent source must not cost the others.
pub async fn poll_once(sources: &mut [Box<dyn Source>], timestamp_ms: u64) -> Vec<Reading> {
    let mut readings = Vec::with_capacity(sources.len());
    for source in sources.iter_mut() {
        match source.read(timestamp_ms).await {
            Ok(reading) => readings.push(reading),
            Err(why) => {
                debug!(source = source.id(), ?why, "source did not answer");
            }
        }
    }
    readings
}

/// Everything the poll loop holds. One struct because the loop needs all of it and a function
/// taking this many arguments is a function nobody calls correctly twice.
pub struct Polling {
    pub sources: Vec<Box<dyn Source>>,
    pub buffer: Arc<Mutex<Queue>>,
    pub batches: Batches,
    pub clock: Arc<dyn Clock + Send + Sync>,
    pub daylight: Sun,
    pub cadence: Cadence,
    pub firmware: String,
}

impl Polling {
    /// How long to wait before the next sweep, and whether that sweep should happen.
    ///
    /// Inside the day's window, the next slot on the grid. Outside it, until the window opens or
    /// the recheck, whichever is sooner — a dark source is not polled, so night costs no timeouts
    /// and no journal noise.
    fn next_step(&self, now_ms: u64) -> (Duration, Option<Now>) {
        let slot = until_next_slot(now_ms, self.cadence.sweep);
        let Ok(millis) = i64::try_from(now_ms) else {
            return (slot, None);
        };
        let Ok(at) = Timestamp::from_millisecond(millis) else {
            return (slot, None);
        };
        match window::now(&self.daylight, at) {
            Ok(now @ Now::Open { .. }) => (slot, Some(now)),
            Ok(now @ Now::Closed { opens }) => {
                let until = at
                    .duration_until(opens)
                    .try_into()
                    .unwrap_or(self.cadence.recheck);
                (until.min(self.cadence.recheck), Some(now))
            }
            // The sun could not be placed. Look again rather than poll blind.
            Err(_) => (self.cadence.recheck, None),
        }
    }

    /// Sweep on the slot grid, inside the window, until stopped.
    pub async fn run(mut self, stop: CancellationToken) {
        // The window is logged when it changes, not every time it is looked at: a line a recheck
        // would be noise, and its absence is what tells a quiet night from a stuck loop.
        let mut was_open: Option<bool> = None;
        let mut first = true;
        loop {
            let (wait, now) = self.next_step(self.clock.now_ms());
            match now {
                Some(now) if was_open != Some(now.is_open()) => {
                    was_open = Some(now.is_open());
                    match now {
                        Now::Open { until } => info!(until = %until, "window open"),
                        Now::Closed { opens } => info!(opens = %opens, "window closed"),
                    }
                }
                Some(Now::Closed { opens }) => debug!(opens = %opens, "window still closed"),
                Some(Now::Open { .. }) => {}
                // The clock or the sun cannot be placed; the recheck is the whole answer.
                None => debug!("cannot place the sun; looking again"),
            }
            if first && now.is_some_and(|now| now.is_open()) {
                info!(seconds = wait.as_secs(), "waiting for the first slot");
            }
            first = false;
            tokio::select! {
                () = sleep(wait) => {}
                () = stop.cancelled() => return,
            }
            if !now.is_some_and(|now| now.is_open()) {
                continue;
            }
            let started = Instant::now();
            let timestamp_ms = self.clock.now_ms();
            let readings = poll_once(&mut self.sources, timestamp_ms).await;
            if readings.is_empty() {
                warn!("no source answered this sweep");
                continue;
            }
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let values: usize = readings.iter().map(|reading| reading.values.len()).sum();
            let depth = self.buffer.lock().map_or(0, |queue| queue.depth());
            info!(sources = readings.len(), values, depth, elapsed_ms, "sweep");
            let beat = heartbeat(self.clock.as_ref(), depth, &self.firmware);
            let batch = self.batches.stamp(readings, beat);
            if let Ok(mut queue) = self.buffer.lock()
                && let Some(dropped) = queue.push(batch)
            {
                warn!(
                    seq = %dropped.seq,
                    dropped = queue.dropped(),
                    "batch dropped; the buffer is full"
                );
            }
        }
    }
}

/// Drain the buffer until it is empty or the cloud stops taking batches, backing off as it goes.
///
/// Runs beside the poll rather than inside it, which is what keeps `DESIGN.md` §7's "polling
/// never waits on the network" true by construction rather than by care.
pub async fn drain_forever(
    buffer: Arc<Mutex<Queue>>,
    cloud: Arc<dyn Cloud>,
    cadence: Cadence,
    stop: CancellationToken,
) {
    let mut backoff = Backoff::new(cadence.backoff_min, cadence.backoff_max);
    loop {
        if stop.is_cancelled() {
            return;
        }
        // `drain_once` journalled the batch's fate, `seq` and all; what is left is the refusal
        // and how long the device stops asking for.
        let outcome = drain_once(&buffer, cloud.as_ref()).await;
        let wait = drain_next_wait(outcome, cadence, &mut backoff);
        if let Some(refused) = outcome.filter(|outcome| !outcome.releases_the_batch()) {
            warn!(outcome = ?refused, ?wait, "the cloud is not taking batches");
        }
        tokio::select! {
            () = sleep(wait) => {}
            () = stop.cancelled() => return,
        }
    }
}

/// How long to wait after one drain attempt, moving the ladder with it. Outside the loop, so
/// what an answer costs is tested without sleeping through it.
fn drain_next_wait(outcome: Option<Outcome>, cadence: Cadence, backoff: &mut Backoff) -> Duration {
    match outcome {
        // Nothing queued: wait for the poll to make something.
        None => {
            backoff.reset();
            cadence.backoff_min
        }
        // Taking batches: keep going while it does, at a pace rather than a burst.
        Some(outcome) if outcome.releases_the_batch() => {
            backoff.reset();
            cadence.drain_pace
        }
        // Refused: wait out this rung, and leave the next one higher.
        Some(_) => backoff.climb(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use async_trait::async_trait;
    use contract::Metric;
    use driver::ReadError;
    use platform::SystemClock;

    use super::*;

    struct Stub {
        id: String,
        metrics: Vec<Metric>,
    }

    impl Stub {
        fn boxed(id: &str, key: &str) -> Box<dyn Source> {
            Box::new(Self {
                id: id.to_owned(),
                metrics: vec![Metric::Gauge {
                    key: key.to_owned(),
                    unit: "W".to_owned(),
                    exponent: -2,
                }],
            })
        }
    }

    #[async_trait]
    impl Source for Stub {
        fn id(&self) -> &str {
            &self.id
        }

        fn metrics(&self) -> &[Metric] {
            &self.metrics
        }

        async fn read(&mut self, _timestamp_ms: u64) -> Result<Reading, ReadError> {
            Err(ReadError::Timeout)
        }
    }

    fn reading(source: &str) -> Reading {
        Reading {
            source: source.to_owned(),
            ts: 1_758_326_400_000,
            values: BTreeMap::from([("power_w".to_owned(), 27_034_i64)]),
        }
    }

    #[test]
    fn the_manifest_is_what_the_sources_declare() {
        let sources = vec![
            Stub::boxed("inverter", "power_w"),
            Stub::boxed("meter", "power_w"),
        ];
        let manifest = manifest_of(&sources);
        assert_eq!(
            manifest
                .sources
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            ["inverter", "meter"]
        );
        // Composed from drivers, so it has to satisfy the contract without anyone checking by eye.
        assert_eq!(manifest.validate(), Ok(()));
    }

    #[test]
    fn a_stamped_batch_satisfies_the_contract() {
        let mut batches = Batches::new("0".repeat(64), "0123456789abcdef".to_owned());
        let batch = batches.stamp(vec![reading("inverter")], Heartbeat::new(1, 0));
        assert_eq!(batch.validate(), Ok(()));
    }

    #[test]
    fn the_counter_starts_at_zero_and_advances_once_per_batch() {
        let mut batches = Batches::new("0".repeat(64), "0123456789abcdef".to_owned());
        let seqs: Vec<String> = (0..4)
            .map(|_| {
                batches
                    .stamp(vec![reading("inverter")], Heartbeat::new(1, 0))
                    .seq
            })
            .collect();
        assert_eq!(seqs, ["0", "1", "2", "3"]);
    }

    #[test]
    fn every_batch_of_one_run_names_the_same_boot() {
        // Half of what identifies a batch. A boot id that changed between batches would make one
        // run look like several and break dedup in the other direction.
        let mut batches = Batches::new("0".repeat(64), "0123456789abcdef".to_owned());
        let first = batches.stamp(vec![reading("inverter")], Heartbeat::new(1, 0));
        let second = batches.stamp(vec![reading("inverter")], Heartbeat::new(1, 0));
        assert_eq!(first.boot_id, second.boot_id);
        assert_ne!(first.seq, second.seq);
    }

    /// A clock stopped at a chosen instant, so a window test is about the window.
    struct Stopped(u64);

    impl Clock for Stopped {
        fn now_ms(&self) -> u64 {
            self.0
        }

        fn uptime_seconds(&self) -> u64 {
            1
        }
    }

    fn polling(now_ms: u64, sources: Vec<Box<dyn Source>>) -> Polling {
        Polling {
            sources,
            buffer: Arc::new(Mutex::new(Queue::new(
                std::num::NonZeroUsize::new(8).unwrap_or(std::num::NonZeroUsize::MIN),
            ))),
            batches: Batches::new("0".repeat(64), "0123456789abcdef".to_owned()),
            clock: Arc::new(Stopped(now_ms)),
            daylight: Sun {
                // São Paulo, where the fixtures were captured.
                site: crate::sun::Site {
                    latitude: -23.55,
                    longitude: -46.63,
                },
                before_sunrise: jiff::SignedDuration::from_mins(30),
                after_sunset: jiff::SignedDuration::from_mins(30),
            },
            cadence: Cadence {
                sweep: Duration::from_secs(300),
                backoff_min: Duration::from_secs(1),
                backoff_max: Duration::from_secs(60),
                drain_pace: Duration::from_millis(100),
                recheck: Duration::from_mins(15),
            },
            firmware: "0.1.0-test".to_owned(),
        }
    }

    /// Milliseconds since the epoch for an ISO instant.
    fn at(iso: &str) -> u64 {
        let ts: jiff::Timestamp = iso.parse().unwrap_or(jiff::Timestamp::UNIX_EPOCH);
        u64::try_from(ts.as_millisecond()).unwrap_or(0)
    }

    #[tokio::test]
    async fn a_source_that_does_not_answer_does_not_cost_the_others() {
        // A failed poll is normal; the buffer carries the gap. One silent source must not take
        // the sweep down with it.
        let mut sources = vec![Stub::boxed("silent", "power_w")];
        let readings = poll_once(&mut sources, 1_758_326_400_000).await;
        assert!(readings.is_empty(), "the stub always fails");
    }

    #[test]
    fn midday_is_inside_the_window_and_sweeps_on_the_slot_grid() {
        let step = polling(at("2026-09-17T15:00:00Z"), Vec::new());
        // 15:00 UTC is midday in São Paulo.
        let (wait, now) = step.next_step(step.clock.now_ms());
        assert!(matches!(now, Some(Now::Open { .. })), "midday must poll");
        assert!(wait <= Duration::from_secs(300));
    }

    #[test]
    fn the_middle_of_the_night_does_not_poll() {
        // The inverter runs on its panels: polling a dark one buys a timeout per source.
        let step = polling(at("2026-09-17T05:00:00Z"), Vec::new());
        let (wait, now) = step.next_step(step.clock.now_ms());
        assert!(
            matches!(now, Some(Now::Closed { .. })),
            "a dark source must not be polled"
        );
        assert!(
            wait > Duration::from_secs(300),
            "and it should wait, not spin: {wait:?}"
        );
    }

    #[test]
    fn a_closed_window_is_looked_at_again_rather_than_slept_through() {
        // The board has no clock until the network steps it, so a sleep until sunrise computed at
        // boot can land hours out. The recheck is what settles it.
        let step = polling(at("2026-09-17T23:30:00Z"), Vec::new());
        let (wait, now) = step.next_step(step.clock.now_ms());
        assert!(matches!(now, Some(Now::Closed { .. })));
        assert!(wait <= step.cadence.recheck, "{wait:?} past the recheck");
    }

    #[test]
    fn slots_are_a_grid_a_restart_rejoins() {
        // Anchored to the epoch, not to when the process started: two devices, or one restarted,
        // land on the same boundaries.
        let period = Duration::from_secs(300);
        assert_eq!(until_next_slot(0, period), period);
        assert_eq!(until_next_slot(1_000, period), Duration::from_secs(299));
        assert_eq!(until_next_slot(299_999, period), Duration::from_millis(1));
        assert_eq!(until_next_slot(300_000, period), period);
    }

    #[test]
    fn a_heartbeat_reports_the_depth_it_was_given() {
        let clock = SystemClock::new();
        let beat = heartbeat(&clock, 7, "0.1.0-test");
        assert_eq!(beat.buffer_depth, 7);
        assert_eq!(beat.firmware_version.as_deref(), Some("0.1.0-test"));
        let mut batches = Batches::new("0".repeat(64), "0123456789abcdef".to_owned());
        assert_eq!(
            batches.stamp(vec![reading("inverter")], beat).validate(),
            Ok(())
        );
    }

    /// A cadence whose rungs are told apart at a glance: 8ms, doubling to a 32ms ceiling.
    fn cadence() -> Cadence {
        Cadence {
            sweep: Duration::from_secs(300),
            backoff_min: Duration::from_millis(8),
            backoff_max: Duration::from_millis(32),
            recheck: Duration::from_mins(15),
            drain_pace: Duration::from_millis(1),
        }
    }

    /// A ladder already climbed to its ceiling, which is where a reset is visible.
    fn climbed(cadence: Cadence) -> Backoff {
        let mut backoff = Backoff::seeded(cadence.backoff_min, cadence.backoff_max, 1);
        for _ in 0..4 {
            backoff.climb();
        }
        backoff
    }

    #[test]
    fn a_cloud_taking_batches_is_paced_rather_than_burst() {
        // Draining as fast as the link allows is a spike at a cloud that has just come back.
        let cadence = cadence();
        for outcome in [Outcome::Committed, Outcome::Rejected(422)] {
            let mut backoff = climbed(cadence);
            let wait = drain_next_wait(Some(outcome), cadence, &mut backoff);
            assert_eq!(wait, cadence.drain_pace, "the backlog burst");
            let next = drain_next_wait(Some(Outcome::Unavailable), cadence, &mut backoff);
            assert!(
                next < cadence.backoff_min,
                "the ladder did not reset: {next:?}"
            );
        }
    }

    #[test]
    fn a_refusal_climbs_the_ladder_rather_than_asking_again_at_once() {
        let cadence = cadence();
        // Spread, so there is no sequence to assert on: each wait belongs to its own rung.
        let mut backoff = Backoff::seeded(cadence.backoff_min, cadence.backoff_max, 1);
        let climbing: Vec<Duration> = (0..5)
            .map(|_| drain_next_wait(Some(Outcome::Unavailable), cadence, &mut backoff))
            .collect();
        assert!(
            climbing
                .iter()
                .all(|wait| *wait >= cadence.backoff_min / 2 && *wait < cadence.backoff_max),
            "a wait left the ladder: {climbing:?}"
        );
        let last = climbing.last().copied().unwrap_or_default();
        assert!(
            last >= cadence.backoff_max / 2,
            "five refusals did not reach the ceiling: {last:?}"
        );
    }

    #[test]
    fn an_empty_buffer_looks_again_rather_than_backing_off() {
        // Nothing queued is not the cloud refusing: the next sweep is what this waits for.
        let cadence = cadence();
        let mut backoff = climbed(cadence);
        assert_eq!(
            drain_next_wait(None, cadence, &mut backoff),
            cadence.backoff_min
        );
    }
}
