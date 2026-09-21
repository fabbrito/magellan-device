//! Layer 5 — the device runtime: config, clock, scheduling, buffer, upload, health.
//!
//! The buffer is what stands between a poor connection and a hole in the record. Bounded on
//! purpose: full, it drops the oldest batch and leaves a visible `seq` gap rather than dying — a
//! health signal, never something hidden.

mod config;
mod device;
mod drain;
mod queue;
pub mod sun;
mod upload;
pub mod window;

use contract::Batch;

pub use crate::config::{Config, Margins, SourceConfig, Token};
pub use crate::device::{Batches, heartbeat, manifest_of};
pub use crate::drain::drain_once;
pub use crate::queue::Queue;
#[cfg(feature = "fake")]
pub use crate::upload::fake;
pub use crate::upload::{Cloud, Declined, Http, Outcome, classify};

/// A bounded, at-least-once queue of batches awaiting upload.
///
/// A batch leaves on `2xx` or a permanent `4xx`; `429`, `503`, `5xx`, a rejected credential
/// (`401`/`403`) and no answer keep it — a rotated token is not worth the readings it would drop.
/// Duplicates are absorbed cloud-side.
pub trait Buffer {
    /// Batches still queued, oldest first — the heartbeat's `buffer_depth`.
    fn depth(&self) -> u32;

    /// Append at the tail. The batch arrives already stamped with its `seq`; when the queue is
    /// full the oldest is dropped, and that spent number never reaching the cloud is the gap.
    fn push(&mut self, batch: Batch);

    /// The oldest queued batch, without removing it.
    fn peek(&self) -> Option<&Batch>;

    /// Drop the oldest batch on `2xx` or a permanent `4xx` — `401`/`403` excepted, which keep it.
    fn pop(&mut self);
}
