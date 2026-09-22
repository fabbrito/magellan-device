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

use crate::window::{self, Now, Sun};
use crate::{Buffer, Cadence, Queue};

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
}
