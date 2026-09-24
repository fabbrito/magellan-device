//! The inverter as a source the runtime polls.
//!
//! The manifest composes itself from the profile: every register the profile names that the
//! contract can carry becomes a metric, so adding a register to the profile adds it to what the
//! cloud stores, with no cloud change and no code change here.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use contract::{Metric, Reading, Resets};
use driver::{ReadError, Source};
use tokio::time::sleep;
use tracing::{debug, warn};

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
        let decoded = self.profile.decode(addr, values);
        if !decoded.implausible.is_empty() {
            // Outside the profile's bounds is garbage on the wire, not a low reading: worth a warn
            // even though the sweep goes on, because the register is telling us something.
            let implausible: Vec<&str> = decoded
                .implausible
                .iter()
                .map(|named| named.name.as_str())
                .collect();
            warn!(
                addr = %format_args!("0x{addr:04X}"),
                implausible = ?implausible,
                "value outside its bounds"
            );
        }
        for named in decoded.values.iter() {
            match &named.value {
                Value::Int(v) if self.is_declared(&named.name) => {
                    into.insert(named.name.clone(), *v);
                }
                // The contract carries integers, and declares only what a metric can hold. A text
                // or undeclared value is read and dropped — the journal is where it goes instead.
                Value::Int(_) | Value::Text(_) => {
                    debug!(metric = %named.name, "decoded but not declared");
                }
            }
        }
    }
}

/// The outcome as one word: `Outcome`'s `Debug` carries whole frames, which is not what a line
/// wants.
const fn outcome_name(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Reply { .. } => "reply",
        Outcome::Refusal { .. } => "refusal",
        Outcome::TimedOut => "timed out",
        Outcome::Lost(_) => "lost",
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
            unit: Some(unit.to_owned()),
            exponent,
            resets: None,
        }),
        // The running total for the day. The cloud takes energy deltas from these — ten times
        // finer than the lifetime totals — and needs the midnight drop declared a reset.
        Some(unit) if entry.name.ends_with("_today") => Some(Metric::Counter {
            key,
            unit: Some(unit.to_owned()),
            exponent,
            resets: Some(Resets::Daily),
        }),
        Some(unit) => Some(Metric::Gauge {
            key,
            unit: Some(unit.to_owned()),
            exponent,
        }),
        // Dimensionless and scaled — a power factor. A scale is what tells it from a state.
        None if exponent != 0 => Some(Metric::Gauge {
            key,
            unit: None,
            exponent,
        }),
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
            .map_err(|e| {
                debug!(addr = %self.addr, error = %e, "connect failed");
                ReadError::Refused(e.to_string())
            })?;
        let mut values = BTreeMap::new();
        let mut last: Option<ReadError> = None;
        for (nth, range) in self.profile.ranges().iter().enumerate() {
            if nth > 0 {
                sleep(self.timing.gap).await;
            }
            let exchange = session
                .read(range.addr, range.qty, self.timing.read)
                .await
                .map_err(|e| {
                    debug!(range = %range.name, error = %e, "read did not go out");
                    ReadError::Refused(e.to_string())
                })?;
            debug!(
                range = %range.name,
                addr = %format_args!("0x{:04X}", range.addr),
                qty = range.qty,
                elapsed_ms = u64::try_from(exchange.elapsed.as_millis()).unwrap_or(u64::MAX),
                outcome = %outcome_name(&exchange.outcome),
                "range read"
            );
            match exchange.outcome {
                Outcome::Reply { rtu, .. } => match registers(&rtu) {
                    Ok(words) => self.absorb(range.addr, &words, &mut values),
                    Err(e) => {
                        debug!(range = %range.name, error = %e, "reply did not decode");
                        last = Some(ReadError::Refused(e.to_string()));
                    }
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
    fn a_dimensionless_ratio_is_a_unitless_gauge() {
        let inverter = inverter();
        for ratio in ["output_power_factor_l1", "meter_power_factor_l1"] {
            assert!(
                matches!(
                    metric(&inverter, ratio),
                    Some(Metric::Gauge { unit: None, exponent, .. }) if exponent != 0
                ),
                "{ratio} is not a unitless gauge"
            );
            assert!(
                inverter.profile.entries().iter().any(|e| e.name == ratio),
                "{ratio} left the profile — this test stopped meaning anything"
            );
        }
    }

    #[test]
    fn a_lifetime_counter_never_resets_and_a_daily_one_resets_daily() {
        let inverter = inverter();
        assert!(matches!(
            metric(&inverter, "energy_total"),
            Some(Metric::Counter { resets: None, .. })
        ));
        let daily: Vec<&str> = inverter
            .profile
            .entries()
            .iter()
            .filter(|e| e.name.ends_with("_today") && e.unit.is_some())
            .map(|e| e.name.as_str())
            .collect();
        assert!(daily.contains(&"generation_time_today"), "{daily:?}");
        for name in daily {
            assert!(
                matches!(
                    metric(&inverter, name),
                    Some(Metric::Counter {
                        resets: Some(Resets::Daily),
                        ..
                    })
                ),
                "{name} is not a daily counter"
            );
        }
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
        assert_eq!((unit.as_deref(), exponent), (Some("V"), -1));
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
            .values
            .iter()
            .map(|r| r.name.as_str())
            .filter(|name| !values.contains_key(*name))
            .collect();
        assert!(dropped.is_empty(), "numeric registers dropped: {dropped:?}");
        let ratios = values.keys().filter(|k| k.contains("power_factor")).count();
        assert_eq!(ratios, 6, "the ratios in this range: {values:?}");
    }

    #[test]
    fn a_chip_code_never_reaches_a_reading() {
        // It decodes — the journal can have it — but a reading carries integers, so it stops
        // here rather than at the contract.
        let inverter = inverter();
        let words = captured(REPLY_0040);
        let decoded = inverter.profile.decode(0x0040, &words);
        assert!(
            matches!(decoded.values.get("protocol_version"), Some(Value::Text(v)) if v == "1.23"),
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
