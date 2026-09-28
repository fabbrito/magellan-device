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
mod run;
mod sun;
mod upload;
mod window;

use crate::buffer::Buffer;
use crate::cadence::Cadence;

pub use crate::config::{Config, SourceConfig, Token};
pub use crate::device::manifest_of;
pub use crate::run::{RunError, Wiring, run};
#[cfg(feature = "fake")]
pub use crate::upload::fake;
pub use crate::upload::{Cloud, Declined, Http, Outcome};
