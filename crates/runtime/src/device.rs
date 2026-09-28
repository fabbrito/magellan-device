//! Assembling what the device sends: the manifest it declares, and the sweeps the buffer stamps.

use std::sync::Arc;
use std::time::{Duration, Instant};

use contract::limits::UPTIME_SECONDS_MAX;
use contract::{Heartbeat, Manifest, Reading};
use driver::Source;
use jiff::Timestamp;
use platform::Clock;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::window::{self, Now, Sun};
use crate::{Buffer, Cadence};

/// The manifest these sources declare, in the order they are polled, in `zone`.
///
/// Composed rather than written down, so adding a source or a register to a driver's profile
/// changes what the cloud stores with no cloud deploy and nothing edited here.
#[must_use]
pub fn manifest_of(zone: &str, sources: &[Box<dyn Source>]) -> Manifest {
    Manifest {
        tz: zone.to_owned(),
        sources: sources
            .iter()
            .map(|source| contract::Source {
                id: source.id().to_owned(),
                metrics: source.metrics().to_vec(),
            })
            .collect(),
    }
}

/// The device's account of itself, as of now.
///
/// Uptime saturates at the contract's bound: a device up that long is healthy, and the buffer
/// asserts the heartbeat it stamps.
fn heartbeat(clock: &dyn Clock, buffer_depth: u32, firmware: &str) -> Heartbeat {
    let uptime = clock.uptime_seconds().min(UPTIME_SECONDS_MAX);
    Heartbeat {
        firmware_version: Some(firmware.to_owned()),
        ..Heartbeat::new(uptime, buffer_depth)
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
    pub buffer: Arc<Buffer>,
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
            // Raced against the stop: a sweep is ranges a gap apart, longer than a service manager
            // waits before it kills. Nothing is stamped until it ends, so a stop loses only it.
            let readings = tokio::select! {
                readings = poll_once(&mut self.sources, timestamp_ms) => readings,
                () = stop.cancelled() => return,
            };
            if readings.is_empty() {
                warn!("no source answered this sweep");
                continue;
            }
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let values: usize = readings.iter().map(|reading| reading.values.len()).sum();
            let sources = readings.len();
            let enqueued = self.buffer.enqueue(readings, |depth| {
                heartbeat(self.clock.as_ref(), depth, &self.firmware)
            });
            for (source, why) in &enqueued.refused {
                warn!(source, %why, "reading refused; dropped");
            }
            let Some(queued) = enqueued.queued else {
                continue;
            };
            info!(sources, values, depth = queued.depth, elapsed_ms, "sweep");
            if let Some(seq) = queued.displaced {
                warn!(
                    %seq,
                    dropped = queued.dropped,
                    "batch dropped; the buffer is full"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use async_trait::async_trait;
    use contract::Metric;
    use driver::ReadError;

    use driver::fake::Fake;
    use platform::fake::Stopped;

    use super::*;

    fn silent(id: &str) -> Box<dyn Source> {
        Box::new(Fake::silent(id, "power_w"))
    }

    #[test]
    fn the_manifest_is_what_the_sources_declare() {
        let sources = vec![silent("inverter"), silent("meter")];
        let manifest = manifest_of("America/Sao_Paulo", &sources);
        assert_eq!(manifest.tz, "America/Sao_Paulo");
        assert_eq!(
            manifest
                .sources
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            ["inverter", "meter"]
        );
        // Composed from drivers, so it has to satisfy the contract without anyone checking by eye.
        assert!(manifest.encode().is_ok());
    }

    /// A source that takes an hour to answer — the slowest sweep there is.
    struct Slow;

    #[async_trait]
    impl Source for Slow {
        fn id(&self) -> &'static str {
            "inverter"
        }

        fn metrics(&self) -> &[Metric] {
            &[]
        }

        async fn read(&mut self, _timestamp_ms: u64) -> Result<Reading, ReadError> {
            sleep(Duration::from_hours(1)).await;
            Err(ReadError::Timeout)
        }
    }

    fn polling(now_ms: u64, sources: Vec<Box<dyn Source>>) -> Polling {
        Polling {
            sources,
            buffer: Arc::new(Buffer::fixture(8)),
            clock: Arc::new(Stopped::at(now_ms)),
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
                backoff_first: Duration::from_secs(1),
                backoff_ceiling: Duration::from_secs(60),
                drain_pace: Duration::from_millis(100),
                recheck: Duration::from_mins(15),
                flush: Duration::from_secs(60),
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
        let mut sources = vec![silent("silent")];
        let readings = poll_once(&mut sources, 1_758_326_400_000).await;
        assert!(readings.is_empty(), "a silent source always times out");
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

    #[tokio::test(start_paused = true)]
    async fn a_stop_mid_sweep_does_not_wait_out_the_sweep() {
        // A real sweep is ranges apart by a gap: a minute or more. A service manager waits less
        // than that before it kills, and a kill journals nothing.
        let polling = polling(at("2026-09-17T15:00:00Z"), vec![Box::new(Slow)]);
        let stop = CancellationToken::new();
        let run = tokio::spawn(polling.run(stop.clone()));
        // Past the first slot, so the sweep is in flight.
        sleep(Duration::from_secs(301)).await;
        stop.cancel();
        let stopped = tokio::time::timeout(Duration::from_secs(1), run).await;
        assert!(
            matches!(stopped, Ok(Ok(()))),
            "the stop waited on the sweep"
        );
    }
}
