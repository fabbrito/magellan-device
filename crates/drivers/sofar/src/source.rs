//! The inverter as a source the runtime polls.
//!
//! The manifest composes itself from the profile: every register the profile names that the
//! contract can carry becomes a metric, so adding a register to the profile adds it to what the
//! cloud stores, with no cloud change and no code change here.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use contract::{Metric, Reading};
use driver::{ReadError, Source};
use tokio::time::sleep;

use crate::decode::Value;
use crate::profile::{Entry, Profile};
use crate::registers;
use crate::session::{Outcome, Session};

/// Shortest gap between reads inside a sweep that has been seen to work.
///
/// Below this the logger wedged into refusing every other read. That was measured against its own
/// framing rather than Modbus TCP, and nothing has shown Modbus TCP is safer — a wedged logger
/// costs a sweep, so the floor stays until a capture says otherwise.
pub const READ_GAP_MIN: Duration = Duration::from_secs(10);

/// How long the driver waits, at each step.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Longest a dial may take before the address counts as dark.
    pub connect: Duration,
    /// Longest one range read may take.
    pub read: Duration,
    /// Pause between reads inside a sweep; see [`READ_GAP_MIN`].
    pub gap: Duration,
}

/// One inverter, read through its logger.
#[derive(Debug)]
pub struct Inverter {
    id: String,
    profile: Profile,
    metrics: Vec<Metric>,
    addr: String,
    slave: u8,
    timing: Timing,
}

impl Inverter {
    /// Declare an inverter at `addr`, reading the registers `profile` names.
    #[must_use]
    pub fn new(id: String, profile: Profile, addr: String, slave: u8, timing: Timing) -> Self {
        let metrics = profile.entries().iter().filter_map(metric_of).collect();
        Self {
            id,
            profile,
            metrics,
            addr,
            slave,
            timing,
        }
    }

    /// Whether the manifest declares this key.
    ///
    /// Scanned rather than indexed: the contract bounds a source at 128 metrics, so the walk is
    /// shorter than the set it would take to avoid it.
    fn is_declared(&self, key: &str) -> bool {
        self.metrics.iter().any(|metric| metric.key() == key)
    }

    /// Decode one range's registers into `into`, keeping only what the manifest declares.
    ///
    /// A register the profile names but the contract cannot carry is decoded and dropped here. It
    /// still reaches the journal; what it must never do is reach a batch, because a batch naming
    /// a metric its manifest does not declare is one the cloud rejects.
    fn absorb(&self, addr: u16, values: &[u16], into: &mut BTreeMap<String, i64>) {
        for reading in self.profile.decode(addr, values).readings.iter() {
            if let Value::Int(v) = reading.value
                && self.is_declared(&reading.name)
            {
                into.insert(reading.name.clone(), v);
            }
        }
    }
}

/// What the contract can carry of one entry, if anything.
fn metric_of(entry: &Entry) -> Option<Metric> {
    if !entry.kind.numeric() {
        // A protocol version or a chip code: the inverter's identity, not a measurement. The
        // contract carries a reading's values as integers, so text has nowhere to go.
        return None;
    }
    let key = entry.name.clone();
    let exponent = entry.exponent;
    match entry.unit.as_deref() {
        Some(unit) if entry.name.ends_with("_total") => Some(Metric::Counter {
            key,
            unit: unit.to_owned(),
            exponent,
        }),
        Some(unit) => Some(Metric::Gauge {
            key,
            unit: unit.to_owned(),
            exponent,
        }),
        // Dimensionless and scaled — a power factor. `Gauge` and `Counter` both require a unit
        // and `State` carries no exponent, so declaring it would mean inventing a unit the
        // register does not have. Decoded and journalled, never declared, until the contract has
        // a shape for a ratio.
        None if exponent != 0 => None,
        // Discrete: an operating state, a fault word, a calendar register. Labels want the fault
        // table from `reference/`, which is its own piece of work.
        None => Some(Metric::State {
            key,
            state_labels: None,
        }),
    }
}

#[async_trait]
impl Source for Inverter {
    fn id(&self) -> &str {
        &self.id
    }

    fn metrics(&self) -> &[Metric] {
        &self.metrics
    }

    async fn read(&mut self, timestamp_ms: u64) -> Result<Reading, ReadError> {
        // The connection lives one sweep. Minutes pass unused between sweeps and the logger is
        // shared, so holding one denies a session to something else for nothing.
        let mut session = Session::connect(&self.addr, self.slave, self.timing.connect)
            .await
            .map_err(|e| ReadError::Refused(e.to_string()))?;
        let mut values = BTreeMap::new();
        let mut last: Option<ReadError> = None;
        for (nth, range) in self.profile.ranges().iter().enumerate() {
            if nth > 0 {
                sleep(self.timing.gap).await;
            }
            let exchange = session
                .read(range.addr, range.qty, self.timing.read)
                .await
                .map_err(|e| ReadError::Refused(e.to_string()))?;
            match exchange.outcome {
                Outcome::Reply { rtu, .. } => match registers(&rtu) {
                    Ok(words) => self.absorb(range.addr, &words, &mut values),
                    Err(e) => last = Some(ReadError::Refused(e.to_string())),
                },
                Outcome::TimedOut => last = Some(ReadError::Timeout),
                Outcome::Refusal { .. } => {
                    last = Some(ReadError::Refused(format!("{} refused", range.name)));
                }
                Outcome::Lost(e) => last = Some(ReadError::Refused(e.to_string())),
            }
        }
        // A sweep that answered in part is still a reading: the gap is in the values, and the
        // cloud stores what arrived. Only a sweep that yielded nothing is a failed poll.
        match (values.is_empty(), last) {
            (true, Some(why)) => Err(why),
            (true, None) => Err(ReadError::Timeout),
            _ => Ok(Reading {
                source: self.id.clone(),
                ts: timestamp_ms,
                values,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{builtin, registers};

    const REPLY_0040: &str = include_str!("../tests/fixtures/tcp-range-0040.hex");
    const REPLY_0400: &str = include_str!("../tests/fixtures/tcp-range-0400.hex");
    const REPLY_0480: &str = include_str!("../tests/fixtures/tcp-range-0480.hex");
    const REPLY_0580: &str = include_str!("../tests/fixtures/tcp-range-0580.hex");
    const REPLY_0680: &str = include_str!("../tests/fixtures/tcp-range-0680.hex");

    fn timing() -> Timing {
        Timing {
            connect: Duration::from_secs(3),
            read: Duration::from_secs(20),
            gap: READ_GAP_MIN,
        }
    }

    fn inverter() -> Inverter {
        Inverter::new(
            "inverter".to_owned(),
            builtin("sofar-g3").expect("the shipped profile parses"),
            "127.0.0.1:8899".to_owned(),
            1,
            timing(),
        )
    }

    /// The registers of a captured MBAP reply: unit id onward is the Modbus body.
    fn captured(hex: &str) -> Vec<u16> {
        let bytes: Vec<u8> = hex
            .split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).expect("fixture is hex"))
            .collect();
        registers(&bytes[6..]).expect("a read reply")
    }

    fn metric(inverter: &Inverter, key: &str) -> Option<Metric> {
        inverter.metrics().iter().find(|m| m.key() == key).cloned()
    }

    #[test]
    fn the_manifest_the_driver_declares_satisfies_the_contract() {
        // The whole point of composing the manifest from the profile: a register added there must
        // not be able to produce a manifest the cloud would reject.
        let inverter = inverter();
        let source = contract::Source {
            id: inverter.id().to_owned(),
            metrics: inverter.metrics().to_vec(),
        };
        source
            .validate()
            .expect("the declared manifest breaks the contract");
    }

    #[test]
    fn a_chip_code_is_not_a_metric() {
        // A reading's values are integers; a version string has nowhere to go.
        let inverter = inverter();
        for identity in ["protocol_version", "comm_mcu_code", "ctrl1_mcu_code"] {
            assert!(
                metric(&inverter, identity).is_none(),
                "{identity} is declared"
            );
        }
    }

    #[test]
    fn a_dimensionless_ratio_is_not_declared() {
        // No unit and a scale of 10^-3: Gauge and Counter need a unit, State carries no exponent.
        // Declaring it would mean inventing a unit the register does not have.
        let inverter = inverter();
        for ratio in ["output_power_factor_l1", "meter_power_factor_l1"] {
            assert!(metric(&inverter, ratio).is_none(), "{ratio} is declared");
            assert!(
                inverter.profile.entries().iter().any(|e| e.name == ratio),
                "{ratio} left the profile — this test stopped meaning anything"
            );
        }
    }

    #[test]
    fn a_lifetime_counter_is_a_counter_and_a_daily_one_is_not() {
        let inverter = inverter();
        assert!(matches!(
            metric(&inverter, "energy_total"),
            Some(Metric::Counter { .. })
        ));
        // Resets at midnight, so it only increases within a day — not what a counter promises.
        assert!(matches!(
            metric(&inverter, "energy_today"),
            Some(Metric::Gauge { .. })
        ));
    }

    #[test]
    fn a_discrete_register_is_a_state() {
        let inverter = inverter();
        for discrete in ["state", "fault1"] {
            assert!(
                matches!(metric(&inverter, discrete), Some(Metric::State { .. })),
                "{discrete} is not a state"
            );
        }
    }

    #[test]
    fn a_gauge_carries_the_profiles_exponent() {
        let inverter = inverter();
        let Some(Metric::Gauge { unit, exponent, .. }) = metric(&inverter, "pv1_voltage") else {
            panic!("pv1_voltage is not a gauge");
        };
        assert_eq!((unit.as_str(), exponent), ("V", -1));
    }

    #[test]
    fn only_declared_keys_reach_a_reading() {
        // A batch naming a metric its manifest does not declare is one the cloud rejects, so the
        // filter is load-bearing rather than tidiness.
        //
        // Every mask bit is set here so that every register in the range decodes, ratios
        // included. The captures have those bits clear — this installation reports no power
        // factor — so real data alone would exercise none of this.
        let inverter = inverter();
        let mut words = vec![0xFFFF_u16; 4];
        words.resize(48, 1);
        let mut values = BTreeMap::new();
        inverter.absorb(0x0480, &words, &mut values);
        for key in values.keys() {
            assert!(inverter.is_declared(key), "{key} is not in the manifest");
        }
        let decoded = inverter.profile.decode(0x0480, &words);
        let dropped: Vec<&str> = decoded
            .readings
            .iter()
            .map(|r| r.name.as_str())
            .filter(|name| !values.contains_key(*name))
            .collect();
        assert!(
            dropped.iter().all(|name| name.contains("power_factor")),
            "something other than a ratio was dropped: {dropped:?}"
        );
        assert_eq!(dropped.len(), 6, "the ratios in this range: {dropped:?}");
    }

    #[test]
    fn a_chip_code_never_reaches_a_reading() {
        // It decodes — the journal can have it — but a reading carries integers, so it stops
        // here rather than at the contract.
        let inverter = inverter();
        let words = captured(REPLY_0040);
        let decoded = inverter.profile.decode(0x0040, &words);
        assert!(
            matches!(decoded.readings.get("protocol_version"), Some(Value::Text(v)) if v == "1.23"),
            "the capture stopped carrying a version string"
        );
        let mut values = BTreeMap::new();
        inverter.absorb(0x0040, &words, &mut values);
        assert!(!values.contains_key("protocol_version"));
    }

    #[test]
    fn a_captured_sweep_names_only_declared_metrics() {
        let inverter = inverter();
        let mut seen = 0;
        for (addr, hex) in [
            (0x0040_u16, REPLY_0040),
            (0x0400, REPLY_0400),
            (0x0480, REPLY_0480),
            (0x0580, REPLY_0580),
            (0x0680, REPLY_0680),
        ] {
            let mut values = BTreeMap::new();
            inverter.absorb(addr, &captured(hex), &mut values);
            for key in values.keys() {
                assert!(inverter.is_declared(key), "{key} is not in the manifest");
            }
            seen += values.len();
        }
        assert!(seen > 0, "no sweep decoded");
    }
}
