//! Why the device will not put a manifest or a batch on the wire.
//!
//! A refusal is an operating error: journalled and dropped. Nothing branches on which rule broke,
//! so the type's job is to read well in one log line and to let a test name the rule it expects.

use std::fmt;

use crate::limits::{
    METRICS_PER_SOURCE_MAX, READINGS_PER_BATCH_MAX, SOURCES_MAX, STATE_LABELS_MAX,
};

/// A list the contract bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counted {
    /// A manifest's sources.
    Sources,
    /// A source's metrics.
    Metrics,
    /// A batch's readings.
    Readings,
    /// A reading's metric values.
    Values,
    /// A state metric's labels.
    StateLabels,
}

impl Counted {
    /// The most the contract takes. One is always the least.
    #[must_use]
    pub const fn max(self) -> usize {
        match self {
            Self::Sources => SOURCES_MAX,
            Self::Metrics | Self::Values => METRICS_PER_SOURCE_MAX,
            Self::Readings => READINGS_PER_BATCH_MAX,
            Self::StateLabels => STATE_LABELS_MAX,
        }
    }
}

/// A string the contract shapes: a length, a pattern, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Named {
    /// A source's id.
    SourceId,
    /// A metric's key.
    MetricKey,
    /// A measured metric's unit.
    Unit,
    /// A state label's code.
    StateCode,
    /// A state label.
    StateLabel,
    /// The SHA-256 a batch names its manifest by.
    ManifestHash,
    /// The lifetime counter, as canonical decimal.
    Seq,
    /// The hex id a boot is known by.
    BootId,
    /// What the device reports it is running.
    FirmwareVersion,
}

/// A number the contract bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Numbered {
    /// A metric's decimal exponent.
    Exponent,
    /// A metric value, before its exponent is applied.
    Value,
    /// A reading's timestamp, in milliseconds since the Unix epoch.
    TimestampMs,
    /// Seconds since the device booted.
    UptimeSeconds,
    /// Batches the device still holds.
    BufferDepth,
    /// Charge left, as a percentage.
    BatteryPercent,
    /// Signal strength, as a percentage.
    Signal,
}

/// What the contract will not take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A list empty or longer than its bound.
    Count {
        /// Which list.
        of: Counted,
        /// How many it held.
        found: usize,
    },
    /// A string the contract's pattern or length bounds refuse.
    Name {
        /// Which string.
        of: Named,
        /// What it was.
        value: String,
    },
    /// A second source with one id, or a second metric with one key.
    Duplicate {
        /// Which string was repeated.
        of: Named,
        /// The value seen twice.
        value: String,
    },
    /// A number past its bound.
    Number {
        /// Which number.
        of: Numbered,
        /// What it was.
        found: i64,
    },
    /// A reading naming a source, or a metric, the manifest never declared.
    Undeclared {
        /// The source the reading named.
        source: String,
        /// The metric key, when the source itself was declared.
        metric: Option<String>,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Count { of, found } => {
                write!(
                    formatter,
                    "{of:?}: {found}, the contract takes 1 to {}",
                    of.max()
                )
            }
            Self::Name { of, value } => write!(formatter, "{of:?}: {value:?} is not the shape"),
            Self::Duplicate { of, value } => write!(formatter, "{of:?}: {value:?} appears twice"),
            Self::Number { of, found } => write!(formatter, "{of:?}: {found} is out of bounds"),
            Self::Undeclared { source, metric } => match metric {
                Some(key) => {
                    write!(formatter, "source {source:?} declares no metric {key:?}")
                }
                None => write!(formatter, "the manifest declares no source {source:?}"),
            },
        }
    }
}

impl std::error::Error for Refusal {}
