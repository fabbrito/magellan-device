//! The device's account of itself, on its own cadence and apart from the batches: a device with
//! nothing to read — an inverter at night — still says it is alive.
//!
//! Fire and forget. A heartbeat is never buffered or retried: the next one supersedes it, and one
//! held back behind an outage would report a state that is no longer true.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use contract::limits::UPTIME_SECONDS_MAX;
use contract::{Heartbeat, Reading};
use platform::Clock;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{Buffer, Cloud, Outcome};

/// When this boot last read each source: the timestamp of its latest reading. Written by the poll,
/// read by the heartbeat.
#[derive(Debug, Default)]
pub struct Heard(Mutex<BTreeMap<String, u64>>);

impl Heard {
    /// Note every source `readings` came from as heard at its reading's timestamp.
    pub fn record(&self, readings: &[Reading]) {
        let mut heard = self.heard();
        for reading in readings {
            heard.insert(reading.source.clone(), reading.ts);
        }
    }

    pub(crate) fn snapshot(&self) -> BTreeMap<String, u64> {
        self.heard().clone()
    }

    /// Each write leaves the map whole, so a poisoned lock still guards a usable one.
    fn heard(&self) -> MutexGuard<'_, BTreeMap<String, u64>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Everything the heartbeat loop holds.
pub struct Beating {
    pub cloud: Arc<dyn Cloud>,
    pub buffer: Arc<Buffer>,
    pub clock: Arc<dyn Clock + Send + Sync>,
    pub heard: Arc<Heard>,
    pub boot_id: String,
    pub firmware: String,
    pub period: Duration,
}

impl Beating {
    /// The device's account of itself, as of now. Uptime saturates at the contract's bound: a
    /// device up that long is healthy.
    fn heartbeat(&self) -> Heartbeat {
        let uptime = self.clock.uptime_seconds().min(UPTIME_SECONDS_MAX);
        Heartbeat {
            firmware_version: Some(self.firmware.clone()),
            sources_last_heard: self.heard.snapshot(),
            ..Heartbeat::new(self.boot_id.clone(), uptime, self.buffer.depth())
        }
    }

    /// One now, so a restart shows at once, then one a period until stopped.
    pub async fn run(self, stop: CancellationToken) {
        loop {
            // Raced against the stop: a request hanging to its timeout must not hold a stop.
            tokio::select! {
                () = self.beat_once() => {}
                () = stop.cancelled() => return,
            }
            tokio::select! {
                () = sleep(self.period) => {}
                () = stop.cancelled() => return,
            }
        }
    }

    async fn beat_once(&self) {
        let heartbeat = self.heartbeat();
        // A last-heard timestamp comes from the clock, which may be wild: an operating error,
        // so this one is skipped and the next tries again.
        if let Err(why) = heartbeat.validate() {
            warn!(%why, "heartbeat refused; skipped");
            return;
        }
        match self.cloud.beat(&heartbeat).await {
            Outcome::Committed => debug!("heartbeat sent"),
            Outcome::Rejected(status) => warn!(status, "the cloud rejected a heartbeat"),
            // The drain journals an outage already; the next heartbeat supersedes this one.
            outcome @ (Outcome::Credential | Outcome::Unavailable) => {
                debug!(?outcome, "heartbeat not taken");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use contract::{Batch, Encoded};
    use platform::fake::Stopped;

    use super::*;
    use crate::Declined;

    /// A cloud that keeps every heartbeat it was sent, answering each with `answer`.
    struct Listening {
        answer: Outcome,
        heard: Mutex<Vec<Heartbeat>>,
    }

    impl Listening {
        fn new(answer: Outcome) -> Arc<Self> {
            Arc::new(Self {
                answer,
                heard: Mutex::new(Vec::new()),
            })
        }

        fn heard(&self) -> Vec<Heartbeat> {
            self.heard.lock().map(|h| h.clone()).unwrap_or_default()
        }
    }

    #[async_trait]
    impl Cloud for Listening {
        async fn declare(&self, manifest: &Encoded) -> Result<String, Declined> {
            Ok(manifest.hash().to_owned())
        }

        async fn send(&self, _batch: &Batch) -> Outcome {
            Outcome::Committed
        }

        async fn beat(&self, heartbeat: &Heartbeat) -> Outcome {
            if let Ok(mut heard) = self.heard.lock() {
                heard.push(heartbeat.clone());
            }
            self.answer
        }
    }

    fn beating(cloud: Arc<dyn Cloud>, buffer: Arc<Buffer>, heard: Arc<Heard>) -> Beating {
        Beating {
            cloud,
            buffer,
            clock: Arc::new(Stopped::at(1_758_326_400_000)),
            heard,
            boot_id: "0123456789abcdef".to_owned(),
            firmware: "0.1.0-test".to_owned(),
            period: Duration::from_hours(1),
        }
    }

    fn reading(source: &str, ts: u64) -> Reading {
        Reading {
            source: source.to_owned(),
            ts,
            values: BTreeMap::from([("power_w".to_owned(), 1_i64)]),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn one_at_start_then_one_a_period_whatever_the_cloud_answers() {
        // Nothing buffered and nothing read, as at night: liveness must not wait on either.
        let cloud = Listening::new(Outcome::Unavailable);
        let stop = CancellationToken::new();
        let task = tokio::spawn(
            beating(cloud.clone(), Arc::new(Buffer::fixture(4)), Arc::default()).run(stop.clone()),
        );
        sleep(Duration::from_mins(150)).await;
        stop.cancel();
        assert!(task.await.is_ok());
        // At 0, 1h and 2h: a refused one is not retried early.
        assert_eq!(cloud.heard().len(), 3);
    }

    #[tokio::test]
    async fn it_names_the_boot_the_backlog_and_every_source_heard() {
        let cloud = Listening::new(Outcome::Committed);
        let buffer = Arc::new(Buffer::fixture(4));
        buffer.fill(2);
        let heard = Arc::new(Heard::default());
        heard.record(&[reading("inverter", 1), reading("meter", 2)]);
        heard.record(&[reading("inverter", 3)]);
        beating(cloud.clone(), buffer, heard).beat_once().await;
        let sent = cloud.heard();
        let [heartbeat] = sent.as_slice() else {
            panic!("one heartbeat, not {}", sent.len());
        };
        assert_eq!(heartbeat.boot_id, "0123456789abcdef");
        assert_eq!(heartbeat.buffer_depth, 2);
        assert_eq!(
            heartbeat.sources_last_heard,
            BTreeMap::from([("inverter".to_owned(), 3), ("meter".to_owned(), 2)]),
            "the latest reading of each"
        );
    }

    #[tokio::test]
    async fn a_heartbeat_the_contract_refuses_is_not_sent() {
        let cloud = Listening::new(Outcome::Committed);
        let heard = Arc::new(Heard::default());
        heard.record(&[reading("inverter", u64::MAX)]);
        beating(cloud.clone(), Arc::new(Buffer::fixture(4)), heard)
            .beat_once()
            .await;
        assert!(cloud.heard().is_empty());
    }
}
