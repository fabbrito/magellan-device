//! Layer 5 — the device runtime: config, clock, scheduling, buffer, upload, health.
//!
//! The buffer is what stands between a poor connection and a hole in the record. Bounded on
//! purpose: full, it drops the oldest batch and leaves a visible `seq` gap rather than dying — a
//! health signal, never something hidden.

mod backoff;
mod buffer;
mod cadence;
mod config;
mod device;
mod drain;
pub mod sun;
mod upload;
pub mod window;

pub use crate::backoff::jitter_seed;
pub use crate::buffer::{Buffer, Enqueued};
pub use crate::cadence::Cadence;
pub use crate::config::{Config, Margins, SourceConfig, Token};
pub use crate::device::{Polling, manifest_of, poll_once};
pub use crate::drain::{declare_forever, drain_forever, drain_once};
#[cfg(feature = "fake")]
pub use crate::upload::fake;
pub use crate::upload::{Cloud, Declined, Http, Outcome, classify, manifest_hash};
