//! Layer 6 — source drivers, one per kind of source a device reads. The runtime knows only this
//! trait, so a new source is a device-side change: the manifest grows and the cloud stores what it
//! declares. The manufacturer's factor table stays in the driver, folding a raw register into the
//! metric's integer value and `exponent`. Sofar-over-Modbus first; a current clamp next.
//!
//! This crate is the seam alone; each kind of source is its own crate beside it, so two drivers
//! cannot grow into each other through a shared module.

use async_trait::async_trait;
use contract::{Metric, Reading};

/// Why a poll produced no reading. A failed poll is normal — the buffer carries the gap — so this
/// is data, not an error the runtime stops on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// Nothing back in time. The source may still answer the next poll.
    Timeout,
    /// The source refused: a Modbus exception, a short frame. Carries what the wire said, for the
    /// journal.
    Refused(String),
}

/// One thing the device polls, and the values it yields. A driver declares its `id` and `metrics`
/// without touching the wire, so the manifest composes on boot from what the hardware reads.
///
/// `Send` because the runtime polls sources from a task of their own; a driver that cannot cross
/// threads cannot be scheduled beside the upload.
#[async_trait]
pub trait Source: Send {
    /// The source id in the manifest. Stable for the life of the driver.
    fn id(&self) -> &str;

    /// What this source declares. Keys unique within the source.
    fn metrics(&self) -> &[Metric];

    /// Poll once. The timestamp is the runtime's clock: the measured instant, not the received one.
    /// The raw register folds here into the metric's integer `value` and `exponent`.
    ///
    /// Async because the transports are: a blocking read inside the executor would stall the
    /// upload and every other source with it. The future is boxed, which is what `dyn Source`
    /// costs until the language boxes it for us.
    ///
    /// # Errors
    ///
    /// Returns `ReadError` when the source is silent or refuses; the buffer carries the gap.
    async fn read(&mut self, timestamp_ms: u64) -> Result<Reading, ReadError>;
}

/// Fakes for tests beside other seams (ADR 5).
#[cfg(feature = "fake")]
pub mod fake {
    use std::collections::BTreeMap;

    use async_trait::async_trait;
    use contract::{Metric, Reading};

    use super::{ReadError, Source};

    /// A source declaring one gauge, answering every poll with the same value, or never.
    #[derive(Debug)]
    pub struct Fake {
        id: String,
        metrics: Vec<Metric>,
        value: Option<i64>,
    }

    impl Fake {
        /// Declares `key`, and reads `value` every poll.
        #[must_use]
        pub fn answering(id: &str, key: &str, value: i64) -> Self {
            Self::declaring(id, key, Some(value))
        }

        /// Declares `key`, and times out every poll: a dark source.
        #[must_use]
        pub fn silent(id: &str, key: &str) -> Self {
            Self::declaring(id, key, None)
        }

        fn declaring(id: &str, key: &str, value: Option<i64>) -> Self {
            Self {
                id: id.to_owned(),
                metrics: vec![Metric::Gauge {
                    key: key.to_owned(),
                    unit: Some("W".to_owned()),
                    exponent: 0,
                }],
                value,
            }
        }
    }

    #[async_trait]
    impl Source for Fake {
        fn id(&self) -> &str {
            &self.id
        }

        fn metrics(&self) -> &[Metric] {
            &self.metrics
        }

        async fn read(&mut self, timestamp_ms: u64) -> Result<Reading, ReadError> {
            let value = self.value.ok_or(ReadError::Timeout)?;
            let values = self
                .metrics
                .iter()
                .map(|metric| (metric.key().to_owned(), value))
                .collect::<BTreeMap<_, _>>();
            Ok(Reading {
                source: self.id.clone(),
                ts: timestamp_ms,
                values,
            })
        }
    }
}
