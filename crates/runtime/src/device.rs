//! Assembling what the device sends: the manifest it declares, and the batches it stamps.

use contract::{Batch, Heartbeat, Manifest, Reading};
use driver::Source;
use platform::Clock;

/// The manifest these sources declare, in the order they are polled.
///
/// Composed rather than written down, so adding a source or a register to a driver's profile
/// changes what the cloud stores with no cloud deploy and nothing edited here.
#[must_use]
pub fn manifest_of(sources: &[Box<dyn Source>]) -> Manifest {
    Manifest {
        sources: sources
            .iter()
            .map(|source| contract::Source {
                id: source.id().to_owned(),
                metrics: source.metrics().to_vec(),
            })
            .collect(),
    }
}

/// Stamps batches for one run of the device.
///
/// The boot id is drawn once and the counter starts at zero, which together identify a batch
/// (ADR 8). The counter advances whether or not a batch is ever delivered — a number spent on a
/// batch the buffer later drops is exactly the gap that makes the loss visible.
#[derive(Debug)]
pub struct Batches {
    manifest_hash: String,
    boot_id: String,
    seq: u64,
}

impl Batches {
    /// Stamp for the manifest the cloud accepted, in the boot `boot_id` names.
    #[must_use]
    pub const fn new(manifest_hash: String, boot_id: String) -> Self {
        Self {
            manifest_hash,
            boot_id,
            seq: 0,
        }
    }

    /// The next batch. Consumes a `seq` even if nothing ever sends it.
    pub fn stamp(&mut self, readings: Vec<Reading>, heartbeat: Option<Heartbeat>) -> Batch {
        let batch = Batch {
            manifest_hash: self.manifest_hash.clone(),
            boot_id: self.boot_id.clone(),
            seq: self.seq.to_string(),
            readings,
            heartbeat,
        };
        // `u64` outlasts any device polling every few minutes. Saturating rather than
        // wrapping so the impossible case repeats one number instead of replaying the
        // whole range against a cloud that deduplicates on it.
        self.seq = self.seq.saturating_add(1);
        batch
    }

    /// The hash these batches name.
    #[must_use]
    pub fn manifest_hash(&self) -> &str {
        &self.manifest_hash
    }
}

/// The device's account of itself, as of now.
#[must_use]
pub fn heartbeat(clock: &dyn Clock, buffer_depth: u32, firmware: &str) -> Heartbeat {
    Heartbeat {
        uptime_seconds: clock.uptime_seconds(),
        buffer_depth,
        battery_percent: None,
        signal: None,
        firmware_version: Some(firmware.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use async_trait::async_trait;
    use contract::Metric;
    use driver::ReadError;
    use platform::SystemClock;

    use super::*;

    struct Stub {
        id: String,
        metrics: Vec<Metric>,
    }

    impl Stub {
        fn boxed(id: &str, key: &str) -> Box<dyn Source> {
            Box::new(Self {
                id: id.to_owned(),
                metrics: vec![Metric::Gauge {
                    key: key.to_owned(),
                    unit: "W".to_owned(),
                    exponent: -2,
                }],
            })
        }
    }

    #[async_trait]
    impl Source for Stub {
        fn id(&self) -> &str {
            &self.id
        }

        fn metrics(&self) -> &[Metric] {
            &self.metrics
        }

        async fn read(&mut self, _timestamp_ms: u64) -> Result<Reading, ReadError> {
            Err(ReadError::Timeout)
        }
    }

    fn reading(source: &str) -> Reading {
        Reading {
            source: source.to_owned(),
            ts: 1_758_326_400_000,
            values: BTreeMap::from([("power_w".to_owned(), 27_034_i64)]),
        }
    }

    #[test]
    fn the_manifest_is_what_the_sources_declare() {
        let sources = vec![
            Stub::boxed("inverter", "power_w"),
            Stub::boxed("meter", "power_w"),
        ];
        let manifest = manifest_of(&sources);
        assert_eq!(
            manifest
                .sources
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            ["inverter", "meter"]
        );
        // Composed from drivers, so it has to satisfy the contract without anyone checking by eye.
        assert_eq!(manifest.validate(), Ok(()));
    }

    #[test]
    fn a_stamped_batch_satisfies_the_contract() {
        let mut batches = Batches::new("0".repeat(64), "0123456789abcdef".to_owned());
        let batch = batches.stamp(vec![reading("inverter")], None);
        assert_eq!(batch.validate(), Ok(()));
    }

    #[test]
    fn the_counter_starts_at_zero_and_advances_once_per_batch() {
        let mut batches = Batches::new("0".repeat(64), "0123456789abcdef".to_owned());
        let seqs: Vec<String> = (0..4)
            .map(|_| batches.stamp(vec![reading("inverter")], None).seq)
            .collect();
        assert_eq!(seqs, ["0", "1", "2", "3"]);
    }

    #[test]
    fn every_batch_of_one_run_names_the_same_boot() {
        // Half of what identifies a batch. A boot id that changed between batches would make one
        // run look like several and break dedup in the other direction.
        let mut batches = Batches::new("0".repeat(64), "0123456789abcdef".to_owned());
        let first = batches.stamp(vec![reading("inverter")], None);
        let second = batches.stamp(vec![reading("inverter")], None);
        assert_eq!(first.boot_id, second.boot_id);
        assert_ne!(first.seq, second.seq);
    }

    #[test]
    fn a_heartbeat_reports_the_depth_it_was_given() {
        let clock = SystemClock::new();
        let beat = heartbeat(&clock, 7, "0.1.0-test");
        assert_eq!(beat.buffer_depth, 7);
        assert_eq!(beat.firmware_version.as_deref(), Some("0.1.0-test"));
        let mut batches = Batches::new("0".repeat(64), "0123456789abcdef".to_owned());
        assert_eq!(
            batches
                .stamp(vec![reading("inverter")], Some(beat))
                .validate(),
            Ok(())
        );
    }
}
