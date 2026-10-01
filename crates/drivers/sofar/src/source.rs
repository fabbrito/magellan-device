//! The inverter as a source the runtime polls.
//!
//! The manifest composes itself from the profile: every register the profile names that the
//! contract can carry becomes a metric, so adding a register to the profile adds it to what the
//! cloud stores, with no cloud change and no code change here.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use contract::{Metric, Reading, Resets};
use driver::{ReadError, Source};
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::profile::common::{Count, count_of};
use crate::profile::decode::Value;
use crate::profile::{Entry, Profile};
use crate::wire::discover;
use crate::wire::modbus::registers;
use crate::wire::session::{Outcome, Session};

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
    /// How long a logger gets to answer the discovery hello. Only a dark one takes it all.
    pub discovery: Duration,
}

/// Where the logger is.
#[derive(Debug, Clone)]
pub enum Locate {
    /// A configured `host:port`, dialled as is.
    Host(String),
    /// Found by broadcasting for the logger's serial when a sweep needs it. Forgotten when a dial
    /// to it fails, so a logger that took a new lease is found again on the next sweep.
    Discover {
        serial: u32,
        port: u16,
        targets: Vec<SocketAddr>,
    },
}

/// One inverter, read through its logger.
#[derive(Debug)]
pub struct Inverter {
    id: String,
    profile: Profile,
    metrics: Vec<Metric>,
    locate: Locate,
    /// The address discovery last found. Never set for [`Locate::Host`].
    found: Option<SocketAddr>,
    slave: u8,
    timing: Timing,
}

impl Inverter {
    /// Declare an inverter found as `locate` says, reading the registers `profile` names.
    ///
    /// Touches nothing: the logger is looked for when a sweep needs it, so a device that boots
    /// while the logger is dark — every night, it runs on the panels — waits like any other night.
    #[must_use]
    pub fn new(id: String, profile: Profile, locate: Locate, slave: u8, timing: Timing) -> Self {
        let metrics = profile.entries().iter().filter_map(metric_of).collect();
        Self {
            id,
            profile,
            metrics,
            locate,
            found: None,
            slave,
            timing,
        }
    }

    #[cfg(test)]
    pub(crate) const fn timing(&self) -> Timing {
        self.timing
    }

    #[cfg(test)]
    pub(crate) const fn locate(&self) -> &Locate {
        &self.locate
    }

    /// Where to dial this sweep: the configured host, or the logger discovery finds.
    async fn address(&mut self) -> Result<String, ReadError> {
        let (serial, port, targets) = match &self.locate {
            Locate::Host(addr) => return Ok(addr.clone()),
            Locate::Discover {
                serial,
                port,
                targets,
            } => (*serial, *port, targets),
        };
        if let Some(found) = self.found {
            return Ok(found.to_string());
        }
        // The serial is never journalled: it names one installation (ADR 9).
        let ip = discover::find(serial, targets, self.timing.discovery)
            .await
            .map_err(|e| {
                debug!(error = %e, "discovery could not broadcast");
                ReadError::Refused(e.to_string())
            })?
            .ok_or_else(|| {
                debug!(source = self.id, "no logger answered discovery");
                ReadError::Timeout
            })?;
        let found = SocketAddr::new(ip, port);
        info!(source = self.id, %found, "found by discovery");
        self.found = Some(found);
        Ok(found.to_string())
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
        Outcome::Refusal => "refusal",
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
        // A `_today` is the running total for the day. The cloud takes energy deltas from these —
        // ten times finer than the lifetime totals — and needs the midnight drop declared a reset.
        Some(unit) if let Some(count) = count_of(&entry.name) => Some(Metric::Counter {
            resets: (count == Count::Daily).then_some(Resets::Daily),
            key,
            unit: Some(unit.to_owned()),
            exponent,
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
        let addr = self.address().await?;
        let mut session = match Session::connect(&addr, self.slave, self.timing.connect).await {
            Ok(session) => session,
            Err(e) => {
                debug!(%addr, error = %e, "connect failed");
                // A discovered address that stops answering may be a new lease, not a dark
                // logger: look again next sweep rather than dial a stale address all day.
                self.found = None;
                return Err(ReadError::Refused(e.to_string()));
            }
        };
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
                Outcome::Refusal => {
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
    use crate::profile::builtin;
    use crate::wire::modbus::registers;
    use crate::wire::session::tests::{Act, fake_logger, hex};

    const REPLY_0040: &str = include_str!("../fixtures/tcp-range-0040.hex");
    const REPLY_0400: &str = include_str!("../fixtures/tcp-range-0400.hex");
    const REPLY_0480: &str = include_str!("../fixtures/tcp-range-0480.hex");
    const REPLY_0580: &str = include_str!("../fixtures/tcp-range-0580.hex");
    const REPLY_0680: &str = include_str!("../fixtures/tcp-range-0680.hex");

    fn timing() -> Timing {
        Timing {
            connect: Duration::from_secs(3),
            read: Duration::from_secs(20),
            gap: READ_GAP_MIN,
            discovery: Duration::from_millis(200),
        }
    }

    fn inverter() -> Inverter {
        Inverter::new(
            "inverter".to_owned(),
            builtin("sofar-g3").expect("the shipped profile parses"),
            Locate::Host("127.0.0.1:8899".to_owned()),
            1,
            timing(),
        )
    }

    /// An inverter that discovers its logger through `targets`, then dials it on `port`.
    fn discovering(targets: Vec<SocketAddr>, port: u16) -> Inverter {
        Inverter::new(
            "inverter".to_owned(),
            builtin("sofar-g3").expect("the shipped profile parses"),
            Locate::Discover {
                serial: 0xDEAD_BEEF,
                port,
                targets,
            },
            1,
            // No gap: these are about finding the logger, not about pacing it.
            Timing {
                gap: Duration::ZERO,
                ..timing()
            },
        )
    }

    /// A loopback port nothing listens on.
    async fn closed_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        listener.local_addr().expect("addr").port()
    }

    /// Our logger answering discovery from loopback.
    async fn our_logger() -> SocketAddr {
        crate::wire::discover::tests::loggers(&[(
            "127.0.0.1",
            "192.0.2.10,ACDE48001122,3735928559",
        )])
        .await
    }

    #[tokio::test]
    async fn a_logger_that_does_not_answer_is_a_timeout_not_a_failed_boot() {
        // Every night: the logger runs on the panels. A sweep without it is a gap, not a crash.
        let silent = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let mut inverter = discovering(vec![silent.local_addr().expect("addr")], 1);
        assert_eq!(inverter.read(0).await, Err(ReadError::Timeout));
        assert_eq!(inverter.found, None);
    }

    #[tokio::test]
    async fn a_discovered_address_that_refuses_a_dial_is_looked_for_again() {
        // A new lease leaves the old address dark. Dialling it all day is a day of gaps.
        let mut inverter = discovering(vec![our_logger().await], closed_port().await);
        assert!(matches!(inverter.read(0).await, Err(ReadError::Refused(_))));
        assert_eq!(inverter.found, None, "the stale address was kept");
    }

    #[tokio::test]
    async fn a_discovered_address_that_took_the_dial_is_kept() {
        // Accepts, then hangs up: the address was right, the sweep was not.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        let mut inverter = discovering(vec![our_logger().await], port);
        assert!(inverter.read(0).await.is_err());
        assert_eq!(
            inverter.found,
            Some(SocketAddr::from(([127, 0, 0, 1], port)))
        );
    }

    /// The shipped profile's read plan, each range beside the reply captured for it.
    const SWEEP: [(u16, &str); 5] = [
        (0x0400, REPLY_0400),
        (0x0480, REPLY_0480),
        (0x0580, REPLY_0580),
        (0x0680, REPLY_0680),
        (0x0040, REPLY_0040),
    ];
    /// Long enough for loopback, short enough that a silent range costs little.
    const QUICK: Duration = Duration::from_millis(250);

    /// An inverter dialling the fake logger at `addr`, with no gap between ranges.
    fn dialling(addr: String) -> Inverter {
        Inverter::new(
            "inverter".to_owned(),
            builtin("sofar-g3").expect("the shipped profile parses"),
            Locate::Host(addr),
            1,
            Timing {
                read: QUICK,
                gap: Duration::ZERO,
                ..timing()
            },
        )
    }

    /// The captured reply's PDU as the fake logger's answer, rewrapped with the request's txn.
    fn answer(capture: &str) -> Act {
        Act::EchoTxn {
            prefix: vec![],
            body: hex(capture)[7..].to_vec(),
        }
    }

    /// What `absorb` makes of the captured replies to the ranges at `addrs`.
    fn absorbed(inverter: &Inverter, addrs: &[u16]) -> BTreeMap<String, i64> {
        let mut values = BTreeMap::new();
        for (addr, capture) in SWEEP.iter().filter(|(addr, _)| addrs.contains(addr)) {
            inverter.absorb(*addr, &captured(capture), &mut values);
        }
        values
    }

    #[test]
    fn the_sweep_is_the_profiles_read_plan() {
        // What the tests below replay in order; a range added to the profile must be captured.
        let plan: Vec<u16> = inverter().profile.ranges().iter().map(|r| r.addr).collect();
        assert_eq!(plan, SWEEP.map(|(addr, _)| addr));
    }

    #[tokio::test]
    async fn a_whole_sweep_is_one_reading_of_every_range() {
        let (addr, _logger) = fake_logger(SWEEP.iter().map(|(_, c)| answer(c)).collect()).await;
        let mut inverter = dialling(addr);
        let reading = inverter.read(1_758_326_400_000).await.expect("a reading");
        assert_eq!(reading.source, "inverter");
        assert_eq!(reading.ts, 1_758_326_400_000);
        assert_eq!(
            reading.values,
            absorbed(&inverter, &SWEEP.map(|(addr, _)| addr))
        );
        assert!(reading.values.contains_key("energy_total"), "{reading:?}");
    }

    #[tokio::test]
    async fn a_sweep_that_answered_in_part_is_still_a_reading() {
        // The gap is in the values; the cloud stores what arrived.
        let script = SWEEP
            .iter()
            .map(|(addr, c)| {
                if *addr == 0x0480 {
                    Act::Silence
                } else {
                    answer(c)
                }
            })
            .collect();
        let (addr, _logger) = fake_logger(script).await;
        let mut inverter = dialling(addr);
        let reading = inverter.read(0).await.expect("a reading");
        assert_eq!(
            reading.values,
            absorbed(&inverter, &[0x0400, 0x0580, 0x0680, 0x0040])
        );
    }

    #[tokio::test]
    async fn a_sweep_nothing_answered_is_a_timeout() {
        let (addr, _logger) = fake_logger(SWEEP.map(|_| Act::Silence).into()).await;
        assert_eq!(dialling(addr).read(0).await, Err(ReadError::Timeout));
    }

    #[tokio::test]
    async fn a_sweep_every_range_refused_is_a_refusal() {
        let refusal = || Act::EchoTxn {
            prefix: vec![],
            body: vec![0x83, 0x02],
        };
        let (addr, _logger) = fake_logger(SWEEP.map(|_| refusal()).into()).await;
        assert!(matches!(
            dialling(addr).read(0).await,
            Err(ReadError::Refused(_))
        ));
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
