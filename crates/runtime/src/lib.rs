//! Layer 5 — the device runtime: config, clock, scheduling, buffer, upload, health.
//!
//! The buffer is what stands between a poor connection and a hole in the record. Bounded on
//! purpose: full, it drops the oldest batch and leaves a visible `seq` gap rather than dying — a
//! health signal, never something hidden.

mod buffer;
mod config;
mod heartbeat;
mod poll;
mod run;
mod schedule;
mod upload;

use crate::buffer::Buffer;
use crate::schedule::cadence::Cadence;

pub use crate::config::{Config, SourceConfig, Token};
pub use crate::poll::manifest_of;
pub use crate::run::{RunError, Wiring, run};
#[cfg(feature = "fake")]
pub use crate::upload::fake;
pub use crate::upload::{Cloud, Declined, Http, Outcome};
