//! Assembling what the device sends: the manifest it declares, and the sweeps the buffer stamps.

use std::sync::Arc;
use std::time::{Duration, Instant};

use contract::{Manifest, Reading};
use driver::Source;
use jiff::Timestamp;
use platform::Clock;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::heartbeat::LastHeard;
use crate::schedule::window::{self, Now, Sun};
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

/// Slots are multiples of `period` since the epoch, so a restart rejoins the same grid rather
/// than starting a new one.
fn until_next_slot(now_ms: u64, period: Duration) -> Duration {
    let period_ms = u64::try_from(period.as_millis()).unwrap_or(u64::MAX).max(1);
    Duration::from_millis(period_ms - now_ms % period_ms)
}

/// Before this instant the wall clock has not been set: the board has no clock of its own until
/// the network steps it, and a reading stamped 1970 would be archived as one. 2026-01-01, before
/// any build that carries it.
const CLOCK_SET_MS_MIN: u64 = 1_767_225_600_000;

/// One sweep: poll every source handed in and keep what answered.
///
/// A source that times out or refuses leaves its readings out and the sweep goes on. A poll that
/// fails is normal — the buffer carries the gap — so one silent source must not cost the others.
pub async fn poll_once<'a>(
    sources: impl IntoIterator<Item = &'a mut Box<dyn Source>>,
    timestamp_ms: u64,
) -> Vec<Reading> {
    let mut readings = Vec::new();
    for source in sources {
        match source.read(timestamp_ms).await {
            Ok(reading) => readings.push(reading),
            Err(why) => {
                debug!(source = source.id(), ?why, "source did not answer");
            }
        }
    }
    readings
}

/// A source, and when it is worth polling. No window is always: the domain is the source's, and
/// only a source that goes dark on a schedule has one.
pub struct Polled {
    pub source: Box<dyn Source>,
    pub window: Option<Sun>,
}

/// Where one source stands at an instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    /// No window: every slot.
    Always,
    Window(Now),
    /// The sun could not be placed. Looked at again rather than polled blind.
    Unplaced,
}

impl Status {
    const fn is_open(self) -> bool {
        matches!(self, Self::Always | Self::Window(Now::Open { .. }))
    }
}

/// What to do next: wait, then sweep the sources open at the start of the wait.
#[derive(Debug)]
struct Step {
    wait: Duration,
    /// One per source, in order. Empty while the clock is not set.
    statuses: Vec<Status>,
}

impl Step {
    fn any_open(&self) -> bool {
        self.statuses.iter().any(|status| status.is_open())
    }
}

/// Everything the poll loop holds. One struct because the loop needs all of it and a function
/// taking this many arguments is a function nobody calls correctly twice.
pub struct Polling {
    pub sources: Vec<Polled>,
    pub buffer: Arc<Buffer>,
    pub clock: Arc<dyn Clock + Send + Sync>,
    pub cadence: Cadence,
    pub heard: Arc<LastHeard>,
}

impl Polling {
    /// How long to wait before the next sweep, and which sources it takes.
    ///
    /// With a source open, the next slot on the grid. With every one closed, until the first
    /// opens or its recheck, whichever is sooner — a dark source is not polled, so night costs no
    /// timeouts and no journal noise.
    fn next_step(&self, now_ms: u64) -> Step {
        let slot = until_next_slot(now_ms, self.cadence.sweep);
        let at = i64::try_from(now_ms)
            .ok()
            .and_then(|millis| Timestamp::from_millisecond(millis).ok());
        let Some(at) = at.filter(|_| now_ms >= CLOCK_SET_MS_MIN) else {
            return Step {
                wait: slot,
                statuses: Vec::new(),
            };
        };
        let mut closed_wait: Option<Duration> = None;
        let mut statuses = Vec::with_capacity(self.sources.len());
        for polled in &self.sources {
            let Some(sun) = polled.window else {
                statuses.push(Status::Always);
                continue;
            };
            let (status, wait) = match window::now(&sun, at) {
                Ok(now @ Now::Open { .. }) => (Status::Window(now), None),
                Ok(now @ Now::Closed { opens }) => {
                    let until = at.duration_until(opens).try_into().unwrap_or(sun.recheck);
                    (Status::Window(now), Some(until.min(sun.recheck)))
                }
                Err(_) => (Status::Unplaced, Some(sun.recheck)),
            };
            statuses.push(status);
            closed_wait = match (closed_wait, wait) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        let step = Step {
            wait: slot,
            statuses,
        };
        if step.any_open() {
            return step;
        }
        Step {
            wait: closed_wait.unwrap_or(slot),
            ..step
        }
    }

    /// A window is journalled when it changes, not every time it is looked at: a line a recheck
    /// would be noise, and its absence is what tells a quiet night from a stuck loop.
    fn run_journal(&self, step: &Step, was_open: &mut [Option<bool>]) {
        if step.statuses.is_empty() {
            debug!("the clock is not set; not sweeping");
        }
        for ((polled, status), was) in self.sources.iter().zip(&step.statuses).zip(was_open) {
            let source = polled.source.id();
            match *status {
                Status::Window(now) if *was != Some(now.is_open()) => {
                    *was = Some(now.is_open());
                    match now {
                        Now::Open { until } => info!(source, until = %until, "window open"),
                        Now::Closed { opens } => info!(source, opens = %opens, "window closed"),
                    }
                }
                Status::Window(Now::Closed { opens }) => {
                    debug!(source, opens = %opens, "window still closed");
                }
                Status::Unplaced => debug!(source, "cannot place the sun; looking again"),
                Status::Window(Now::Open { .. }) | Status::Always => {}
            }
        }
    }

    /// Sweep on the slot grid, each source inside its window, until stopped.
    pub async fn run(mut self, stop: CancellationToken) {
        let mut was_open: Vec<Option<bool>> = vec![None; self.sources.len()];
        let mut first = true;
        loop {
            let next = self.next_step(self.clock.now_ms());
            self.run_journal(&next, &mut was_open);
            if first && next.any_open() {
                info!(seconds = next.wait.as_secs(), "waiting for the first slot");
            }
            first = false;
            tokio::select! {
                () = sleep(next.wait) => {}
                () = stop.cancelled() => return,
            }
            if !next.any_open() {
                continue;
            }
            let started = Instant::now();
            let timestamp_ms = self.clock.now_ms();
            // Collected before the await: an iterator adapter held across it is a closure whose
            // lifetimes the compiler cannot prove `Send`.
            let mut open: Vec<&mut Box<dyn Source>> = Vec::with_capacity(self.sources.len());
            for (polled, status) in self.sources.iter_mut().zip(&next.statuses) {
                if status.is_open() {
                    open.push(&mut polled.source);
                }
            }
            // Raced against the stop: a sweep is ranges a gap apart, longer than a service manager
            // waits before it kills. Nothing is stamped until it ends, so a stop loses only it.
            let readings = tokio::select! {
                readings = poll_once(open, timestamp_ms) => readings,
                () = stop.cancelled() => return,
            };
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            self.run_enqueue(readings, elapsed_ms);
        }
    }

    fn run_enqueue(&self, readings: Vec<Reading>, elapsed_ms: u64) {
        if readings.is_empty() {
            warn!("no source answered this sweep");
            return;
        }
        self.heard.record(&readings);
        let values: usize = readings.iter().map(|reading| reading.values.len()).sum();
        let sources = readings.len();
        let enqueued = self.buffer.enqueue(readings);
        for (source, why) in &enqueued.refused {
            warn!(source, %why, "reading refused; dropped");
        }
        let Some(queued) = enqueued.queued else {
            return;
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

    /// São Paulo, where the fixtures were captured, with half-hour margins.
    fn sun() -> Sun {
        Sun {
            site: crate::schedule::sun::Site {
                latitude: -23.55,
                longitude: -46.63,
            },
            before_sunrise: jiff::SignedDuration::from_mins(30),
            after_sunset: jiff::SignedDuration::from_mins(30),
            recheck: Duration::from_mins(15),
        }
    }

    fn windowed(source: Box<dyn Source>) -> Polled {
        Polled {
            source,
            window: Some(sun()),
        }
    }

    fn always(source: Box<dyn Source>) -> Polled {
        Polled {
            source,
            window: None,
        }
    }

    fn polling(now_ms: u64, sources: Vec<Polled>) -> Polling {
        Polling {
            sources,
            buffer: Arc::new(Buffer::fixture(8)),
            clock: Arc::new(Stopped::at(now_ms)),
            cadence: Cadence {
                sweep: Duration::from_secs(300),
                backoff_first: Duration::from_secs(1),
                backoff_ceiling: Duration::from_secs(60),
                drain_pace: Duration::from_millis(100),
                heartbeat: Duration::from_hours(1),
            },
            heard: Arc::default(),
        }
    }

    /// Milliseconds since the epoch for an ISO instant.
    fn at(iso: &str) -> u64 {
        let ts: jiff::Timestamp = iso.parse().unwrap_or(jiff::Timestamp::UNIX_EPOCH);
        u64::try_from(ts.as_millisecond()).unwrap_or(0)
    }

    fn step_at(iso: &str, sources: Vec<Polled>) -> Step {
        let polling = polling(at(iso), sources);
        polling.next_step(polling.clock.now_ms())
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
        // 15:00 UTC is midday in São Paulo.
        let step = step_at("2026-09-17T15:00:00Z", vec![windowed(silent("inverter"))]);
        assert!(matches!(
            step.statuses[..],
            [Status::Window(Now::Open { .. })]
        ));
        assert!(step.wait <= Duration::from_secs(300));
    }

    #[test]
    fn the_middle_of_the_night_does_not_poll() {
        // The inverter runs on its panels: polling a dark one buys a timeout per source.
        let step = step_at("2026-09-17T05:00:00Z", vec![windowed(silent("inverter"))]);
        assert!(!step.any_open(), "a dark source must not be polled");
        assert!(
            step.wait > Duration::from_secs(300),
            "and it should wait, not spin: {:?}",
            step.wait
        );
    }

    #[test]
    fn a_closed_window_is_looked_at_again_rather_than_slept_through() {
        // The board has no clock until the network steps it, so a sleep until sunrise computed at
        // boot can land hours out. The recheck is what settles it.
        let step = step_at("2026-09-17T23:30:00Z", vec![windowed(silent("inverter"))]);
        assert!(matches!(
            step.statuses[..],
            [Status::Window(Now::Closed { .. })]
        ));
        assert!(
            step.wait <= sun().recheck,
            "{:?} past the recheck",
            step.wait
        );
    }

    #[test]
    fn a_source_without_a_window_sweeps_through_the_night() {
        // Liveness and the domain are apart: a sensor with no sun in it reads at midnight.
        let step = step_at(
            "2026-09-17T05:00:00Z",
            vec![windowed(silent("inverter")), always(silent("meter"))],
        );
        assert_eq!(step.statuses.len(), 2);
        assert!(!step.statuses[0].is_open(), "the inverter is dark");
        assert!(step.statuses[1].is_open(), "the meter is not");
        assert!(step.wait <= Duration::from_secs(300), "on the slot grid");
    }

    #[test]
    fn nothing_sweeps_before_the_clock_is_set() {
        // A reading stamped 1970 would be archived as one; the next slot looks again.
        let polling = polling(1_000, vec![always(silent("meter"))]);
        let step = polling.next_step(polling.clock.now_ms());
        assert!(!step.any_open());
        assert!(step.wait <= Duration::from_secs(300));
    }

    #[tokio::test(start_paused = true)]
    async fn a_sweep_polls_only_the_sources_open() {
        let sources = vec![
            windowed(Box::new(Fake::answering("inverter", "power_w", 1))),
            always(Box::new(Fake::answering("meter", "power_w", 2))),
        ];
        let polling = polling(at("2026-09-17T05:00:00Z"), sources);
        let heard = Arc::clone(&polling.heard);
        let stop = CancellationToken::new();
        let run = tokio::spawn(polling.run(stop.clone()));
        sleep(Duration::from_secs(301)).await;
        stop.cancel();
        assert!(run.await.is_ok());
        let read: Vec<String> = heard.snapshot().into_keys().collect();
        assert_eq!(read, ["meter"], "the inverter's window is closed");
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
        let polling = polling(at("2026-09-17T15:00:00Z"), vec![windowed(Box::new(Slow))]);
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
