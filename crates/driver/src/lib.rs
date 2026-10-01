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

#[cfg(feature = "fake")]
pub mod fake;
