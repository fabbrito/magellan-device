//! The seam: Layer 4 as the device implements it. The authority is the cloud's published `OpenAPI`
//! document; this crate is a hand-written native reading of its schemas, so neither side's
//! toolchain constrains the other, and a disagreement is a bug here.
//!
//! These types are read from the published document. Where the two disagree the document wins.
//!
//! A measured metric's `exponent` makes every reading value an integer: the physical value is
//! `value × 10^exponent`, and a finite-decimal manufacturer factor folds into the pair exactly.
//! Metric keys are unique within a source — a rule the document carries only as description text.
//!
//! Hash the exact bytes sent: a re-serialization that differs by a byte is the failure the cloud's
//! `ETag` catches.

pub mod limits;
mod refusal;
mod validate;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use crate::refusal::{Counted, Named, Numbered, Refusal};

/// The published document's `info.version` these types were read from. Its major is the `/vN` in
/// every path; a minor adds what the device may ignore, a patch changes descriptions only — so only
/// a new major is a change here.
pub const CONTRACT_VERSION: &str = "1.0.0";
pub use crate::validate::{key_is_well_formed, zone_is_known};

/// A device's declaration of its zone, its sources and their metrics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// IANA time zone, e.g. `America/Sao_Paulo`. Calendar days are cut in it.
    pub tz: String,
    /// One to [`limits::SOURCES_MAX`] sources, each id unique.
    pub sources: Vec<Source>,
}

/// A named thing a device polls, and the metrics it declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// Stable within the device. Pattern-bound ASCII.
    pub id: String,
    /// One to 128 metrics, each key unique within the source.
    pub metrics: Vec<Metric>,
}

/// A named, typed quantity of a source. A gauge and a counter are measured and carry a decimal
/// exponent, and a unit when they have one; a state is a discrete condition and carries neither.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// Unknown fields denied: the document's metrics are strict, so a gauge carrying `resets` is a 400.
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Metric {
    /// A value in time.
    Gauge {
        /// Pattern-bound ASCII, unique within the source.
        key: String,
        /// What the value is in. Absent for a ratio — or a unit the producer cannot name, which
        /// is the same on the wire.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
        /// `-12..12`; the value is `value × 10^exponent`.
        exponent: i8,
    },
    /// Monotonic between resets. Any decrease is a reset, declared or not — the cloud finds each by
    /// the drop, so no boundary crosses the wire.
    Counter {
        /// Pattern-bound ASCII, unique within the source.
        key: String,
        /// What the value is in. Absent for a ratio — or a unit the producer cannot name, which
        /// is the same on the wire.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
        /// `-12..12`; the value is `value × 10^exponent`.
        exponent: i8,
        /// The cadence it resets on, when it has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resets: Option<Resets>,
    },
    /// A discrete condition.
    State {
        /// Pattern-bound ASCII, unique within the source.
        key: String,
        /// Up to [`limits::STATE_LABELS_MAX`] labels, keyed by decimal codes of at most
        /// [`limits::STATE_CODE_DIGITS_MAX`] digits.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state_labels: Option<BTreeMap<String, String>>,
    },
}

/// A counter's declared reset cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resets {
    /// A running total for the day.
    Daily,
}

/// One source poll: a UTC timestamp in ms and that source's metric values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reading {
    /// The `Source::id` this poll read.
    pub source: String,
    /// Milliseconds since the Unix epoch, UTC.
    pub ts: u64,
    /// One to 128 integer values, keyed by metric key. The metric's `exponent` scales each.
    pub values: BTreeMap<String, i64>,
}

/// The device's account of itself, sent on its own cadence and apart from any batch: a device with
/// nothing to read still says it is alive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {
    /// The boot this account is from, as its batches carry it. What makes an uptime reset
    /// explainable.
    pub boot_id: String,
    /// Seconds since boot. Resets on every reboot, power cut and OTA.
    pub uptime_seconds: u64,
    /// Batches the device still holds.
    pub buffer_depth: u32,
    /// Optional: the platform may not know it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_percent: Option<u8>,
    /// Optional: 0 to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_percent: Option<u8>,
    /// Optional: what the device is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware_version: Option<String>,
    /// When this boot last read each source, in milliseconds since the Unix epoch — one hop down
    /// the chain, as the cloud's own last-heard is one hop up. A source not read since boot is
    /// absent, so an empty map is a boot with nothing read yet.
    pub sources_last_heard: BTreeMap<String, u64>,
}

impl Heartbeat {
    /// A heartbeat carrying what the contract requires; the rest is a platform's to fill.
    #[must_use]
    pub const fn new(boot_id: String, uptime_seconds: u64, buffer_depth: u32) -> Self {
        Self {
            boot_id,
            uptime_seconds,
            buffer_depth,
            battery_percent: None,
            signal_percent: None,
            firmware_version: None,
            sources_last_heard: BTreeMap::new(),
        }
    }
}

/// One upload: a `seq`, a manifest hash, ordered readings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    /// SHA-256 of the manifest's bytes as sent, lowercase hex.
    pub manifest_hash: String,
    /// Hex, 8 to 32 digits, drawn once per boot and needing no flash to keep. With `seq`, what
    /// makes a gap visible; the cloud deduplicates on each reading's source and `ts`, not this.
    pub boot_id: String,
    /// A counter, monotonic within one boot, sent as canonical decimal: no leading zeros, at most
    /// `u64::MAX`. Paired with `boot_id`, so it may restart from zero after a reboot without the
    /// device writing anything to flash.
    pub seq: String,
    /// One to 512 readings, in the order they were polled.
    pub readings: Vec<Reading>,
}

/// A manifest as sent: bytes that passed the contract, and their hash. Only [`Manifest::encode`]
/// makes one, so what is sized, hashed and sent is one serialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    bytes: Vec<u8>,
    hash: String,
}

impl Encoded {
    /// The body of the declaration.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// [`manifest_hash`] of [`Encoded::bytes`]: the name every batch read under it carries.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }
}

/// SHA-256 over the exact manifest bytes, lowercase hex.
///
/// The cloud recomputes it and returns the accepted hash in `ETag`, so hashing other bytes than
/// were sent surfaces at the first exchange, not as an unknown hash retried forever.
#[must_use]
pub fn manifest_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The bytes the cloud's hash oracle pins. Predate `tz`: bytes only, no longer a manifest.
    const ORACLE_BYTES: &str = concat!(
        r#"{"sources":[{"id":"source_1","metrics":["#,
        r#"{"key":"power_w","kind":"gauge","unit":"W","exponent":-2}]}]}"#,
    );

    // The bytes a device sends for a one-source manifest.
    const MANIFEST_JSON: &str = concat!(
        r#"{"tz":"America/Sao_Paulo","sources":[{"id":"source_1","metrics":["#,
        r#"{"kind":"gauge","key":"power_w","unit":"W","exponent":-2}]}]}"#,
    );

    #[test]
    fn manifest_hash_matches_the_clouds_oracle() {
        assert_eq!(
            manifest_hash(ORACLE_BYTES.as_bytes()),
            "d935aec39b4c492681d137f322ce5876ce1509289a3d5d759cd0b85fbf11790a"
        );
    }

    #[test]
    fn the_bytes_hashed_are_the_bytes_sent() {
        let manifest: Manifest = serde_json::from_str(MANIFEST_JSON).unwrap();
        let encoded = manifest.encode().unwrap();
        assert_eq!(encoded.bytes(), MANIFEST_JSON.as_bytes());
        assert_eq!(encoded.hash(), manifest_hash(MANIFEST_JSON.as_bytes()));
    }

    #[test]
    fn manifest_hash_of_empty_bytes() {
        assert_eq!(
            manifest_hash(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn batch_round_trips_the_wire_shape() {
        let json = r#"{
            "manifest_hash": "d935aec39b4c492681d137f322ce5876ce1509289a3d5d759cd0b85fbf11790a",
            "boot_id": "0123456789abcdef",
            "seq": "1",
            "readings": [
                { "source": "source_1", "ts": 1758326400000, "values": { "power_w": 27034 } }
            ]
        }"#;
        let batch: Batch = serde_json::from_str(json).unwrap();
        assert_eq!(
            (batch.boot_id.as_str(), batch.seq.as_str()),
            ("0123456789abcdef", "1")
        );
        assert_eq!(batch.readings.len(), 1);
        assert_eq!(batch.readings[0].values["power_w"], 27034);
        let round = serde_json::to_string(&batch).unwrap();
        assert_eq!(serde_json::from_str::<Batch>(&round).unwrap(), batch);
    }

    #[test]
    fn a_batch_carrying_a_heartbeat_does_not_parse() {
        // The heartbeat left the batch; the cloud refuses a batch that still carries one.
        let json = r#"{"manifest_hash":"0","boot_id":"0","seq":"1","readings":[],
            "heartbeat":{"boot_id":"0","uptime_seconds":1,"buffer_depth":0,"sources_last_heard":{}}}"#;
        assert!(serde_json::from_str::<Batch>(json).is_err());
    }

    #[test]
    fn heartbeat_round_trips_the_wire_shape() {
        let json = r#"{
            "boot_id": "0123456789abcdef",
            "uptime_seconds": 42,
            "buffer_depth": 3,
            "sources_last_heard": { "inverter": 1758326400000 }
        }"#;
        let heartbeat: Heartbeat = serde_json::from_str(json).unwrap();
        assert_eq!(heartbeat.sources_last_heard["inverter"], 1_758_326_400_000);
        // Absent optionals stay absent: JSON Schema forbids the extra `null`. An empty map is
        // still sent: the document requires the field.
        let round =
            serde_json::to_string(&Heartbeat::new("0123456789abcdef".to_owned(), 1, 0)).unwrap();
        assert!(!round.contains("battery_percent"), "{round}");
        assert!(round.contains(r#""sources_last_heard":{}"#), "{round}");
        assert_eq!(serde_json::from_str::<Heartbeat>(json).unwrap(), heartbeat);
    }

    #[test]
    fn metric_round_trips_each_kind() {
        let gauge: Manifest = serde_json::from_str(MANIFEST_JSON).unwrap();
        assert_eq!(
            gauge.sources[0].metrics,
            vec![Metric::Gauge {
                key: "power_w".to_owned(),
                unit: Some("W".to_owned()),
                exponent: -2,
            }]
        );

        let mixed = Manifest {
            tz: "UTC".to_owned(),
            sources: vec![Source {
                id: "source_1".to_owned(),
                metrics: vec![
                    Metric::Counter {
                        key: "energy_wh".to_owned(),
                        unit: Some("Wh".to_owned()),
                        exponent: -3,
                        resets: None,
                    },
                    Metric::Counter {
                        key: "energy_today_wh".to_owned(),
                        unit: Some("Wh".to_owned()),
                        exponent: -3,
                        resets: Some(Resets::Daily),
                    },
                    Metric::Gauge {
                        key: "power_factor".to_owned(),
                        unit: None,
                        exponent: -2,
                    },
                    Metric::State {
                        key: "status".to_owned(),
                        state_labels: Some(BTreeMap::from([("0".to_owned(), "idle".to_owned())])),
                    },
                ],
            }],
        };
        let round = serde_json::to_string(&mixed).unwrap();
        // One `unit` per counter, one `resets` for the daily one. JSON Schema forbids a `null`
        // where the key is absent.
        assert_eq!(round.matches("\"unit\"").count(), 2, "{round}");
        assert_eq!(round.matches("\"resets\"").count(), 1, "{round}");
        assert!(round.contains("\"resets\":\"daily\""), "{round}");
        assert_eq!(serde_json::from_str::<Manifest>(&round).unwrap(), mixed);
    }

    #[test]
    fn a_manifest_without_a_zone_does_not_parse() {
        assert!(serde_json::from_str::<Manifest>(ORACLE_BYTES).is_err());
        let manifest: Manifest = serde_json::from_str(MANIFEST_JSON).unwrap();
        assert_eq!(manifest.tz, "America/Sao_Paulo");
    }

    // Only a counter resets; the cloud answers `resets` anywhere else with a 400.
    #[test]
    fn resets_off_a_counter_does_not_parse() {
        for json in [
            r#"{"kind":"gauge","key":"k","exponent":0,"resets":"daily"}"#,
            r#"{"kind":"state","key":"k","resets":"daily"}"#,
        ] {
            assert!(
                serde_json::from_str::<Metric>(json).is_err(),
                "{json} parsed"
            );
        }
        let counter = r#"{"kind":"counter","key":"k","exponent":0,"resets":"daily"}"#;
        assert!(serde_json::from_str::<Metric>(counter).is_ok());
    }
}
