//! What the device checks before a manifest or a batch reaches the buffer.
//!
//! The patterns are written out rather than compiled: a regex engine is a dependency this crate
//! does not need for shapes that never change.
//!
//! Nothing here asserts. A driver handing back an out-of-range register is operating data, so the
//! answer is a refusal the caller journals — what the runtime built itself, the runtime asserts.

use std::collections::BTreeMap;

use crate::limits::{
    BATTERY_PERCENT_MAX, BOOT_ID_LENGTH_MAX, BOOT_ID_LENGTH_MIN, BUFFER_DEPTH_MAX, EXPONENT_MAX,
    EXPONENT_MIN, FIRMWARE_VERSION_LENGTH_MAX, KEY_LENGTH_MAX, MANIFEST_BYTES_MAX,
    MANIFEST_HASH_HEX_LENGTH, METRIC_VALUE_MAX, METRIC_VALUE_MIN, SEQ_DIGITS_MAX,
    SIGNAL_PERCENT_MAX, STATE_CODE_DIGITS_MAX, STATE_LABEL_LENGTH_MAX, TIMESTAMP_MS_MAX,
    TZ_LENGTH_MAX, UNIT_LENGTH_MAX, UPTIME_SECONDS_MAX,
};
use crate::refusal::{Counted, Named, Numbered, Refusal};
use crate::{Batch, Heartbeat, Manifest, Metric, Reading, Source};

/// A list within `1..=max`.
fn count_within(of: Counted, found: usize) -> Result<(), Refusal> {
    if found >= 1 && found <= of.max() {
        return Ok(());
    }
    Err(Refusal::Count { of, found })
}

/// A number within its bound, named so the journal says which.
fn number_within(of: Numbered, found: i64, min: i64, max: i64) -> Result<(), Refusal> {
    if found >= min && found <= max {
        return Ok(());
    }
    Err(Refusal::Number { of, found })
}

fn refuse_name(of: Named, value: &str) -> Refusal {
    Refusal::Name {
        of,
        value: value.to_owned(),
    }
}

/// Whether a source id or metric key matches the contract's pattern.
///
/// The shape: ASCII, opening on an alphanumeric or an underscore, then dots and hyphens as well —
/// URL-unreserved only, since the cloud's read API takes each as a path segment unescaped. Bytes
/// are characters because the pattern admits nothing wider.
///
/// Public so a device can refuse a bad id where it is configured rather than at the first upload.
#[must_use]
pub fn key_is_well_formed(key: &str) -> bool {
    let mut characters = key.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() && first != '_' {
        return false;
    }
    if key.len() > KEY_LENGTH_MAX {
        return false;
    }
    characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Whether `zone` fits the contract's pattern and is in the bundled tz database, spelled as the
/// database spells it — its lookup ignores case, and two spellings are two manifest hashes.
///
/// Public so a device can refuse a zone where it is configured rather than at the first upload.
#[must_use]
pub fn zone_is_known(zone: &str) -> bool {
    if zone.len() > TZ_LENGTH_MAX || !zone.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return false;
    }
    let segment = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-'))
    };
    if !zone.split('/').all(segment) {
        return false;
    }
    jiff_tzdb::get(zone).is_some_and(|(name, _)| name == zone)
}

/// Lowercase hex of an exact or a bounded length.
fn hex_is_well_formed(value: &str, length_min: usize, length_max: usize) -> bool {
    if value.len() < length_min || value.len() > length_max {
        return false;
    }
    value
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f'))
}

/// Decimal digits, at least one, at most `digits_max`.
fn digits_are_well_formed(value: &str, digits_max: usize) -> bool {
    !value.is_empty() && value.len() <= digits_max && value.chars().all(|c| c.is_ascii_digit())
}

/// Canonical decimal: no leading zero unless the whole number is zero, and inside a `u64`. Leading
/// zeros would spell one `seq` two ways, and gap detection would then misread the sequence.
fn seq_is_well_formed(seq: &str) -> bool {
    if !digits_are_well_formed(seq, SEQ_DIGITS_MAX) {
        return false;
    }
    if seq.starts_with('0') && seq.len() > 1 {
        return false;
    }
    seq.parse::<u64>().is_ok()
}

/// A string within `1..=length_max` bytes.
fn text_within(of: Named, value: &str, length_max: usize) -> Result<(), Refusal> {
    if value.is_empty() || value.len() > length_max {
        return Err(refuse_name(of, value));
    }
    Ok(())
}

impl Metric {
    /// The key this metric declares, whatever its kind.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::Gauge { key, .. } | Self::Counter { key, .. } | Self::State { key, .. } => key,
        }
    }

    /// Check this metric against the contract.
    ///
    /// # Errors
    ///
    /// Returns the first rule the metric breaks.
    pub fn validate(&self) -> Result<(), Refusal> {
        if !key_is_well_formed(self.key()) {
            return Err(refuse_name(Named::MetricKey, self.key()));
        }
        match self {
            Self::Gauge { unit, exponent, .. } | Self::Counter { unit, exponent, .. } => {
                if let Some(unit) = unit {
                    text_within(Named::Unit, unit, UNIT_LENGTH_MAX)?;
                }
                number_within(
                    Numbered::Exponent,
                    i64::from(*exponent),
                    i64::from(EXPONENT_MIN),
                    i64::from(EXPONENT_MAX),
                )
            }
            Self::State { state_labels, .. } => match state_labels {
                Some(labels) => validate_state_labels(labels),
                None => Ok(()),
            },
        }
    }
}

/// Labels keyed by decimal codes, each label non-empty and bounded.
fn validate_state_labels(labels: &BTreeMap<String, String>) -> Result<(), Refusal> {
    count_within(Counted::StateLabels, labels.len())?;
    for (code, label) in labels {
        if !digits_are_well_formed(code, STATE_CODE_DIGITS_MAX) {
            return Err(refuse_name(Named::StateCode, code));
        }
        text_within(Named::StateLabel, label, STATE_LABEL_LENGTH_MAX)?;
    }
    Ok(())
}

impl Source {
    /// Check this source and every metric it declares.
    ///
    /// # Errors
    ///
    /// Returns the first rule the source breaks, a repeated metric key included.
    pub fn validate(&self) -> Result<(), Refusal> {
        if !key_is_well_formed(&self.id) {
            return Err(refuse_name(Named::SourceId, &self.id));
        }
        count_within(Counted::Metrics, self.metrics.len())?;

        // Scanned against what came before rather than indexed: the list is bounded at
        // METRICS_PER_SOURCE_MAX, so the comparisons cost less than the allocation a set needs.
        for (position, metric) in self.metrics.iter().enumerate() {
            metric.validate()?;
            let key = metric.key();
            let mut earlier = self.metrics.iter().take(position);
            if earlier.any(|other| other.key() == key) {
                return Err(Refusal::Duplicate {
                    of: Named::MetricKey,
                    value: key.to_owned(),
                });
            }
        }
        Ok(())
    }
}

impl Manifest {
    /// Check this manifest against the contract, before it is hashed and sent.
    ///
    /// # Errors
    ///
    /// Returns the first rule the manifest breaks, a repeated source id included.
    pub fn validate(&self) -> Result<(), Refusal> {
        if !zone_is_known(&self.tz) {
            return Err(refuse_name(Named::Zone, &self.tz));
        }
        count_within(Counted::Sources, self.sources.len())?;

        // Bounded at SOURCES_MAX, so the same scan as a source's metrics, and no allocation.
        for (position, source) in self.sources.iter().enumerate() {
            source.validate()?;
            let id = source.id.as_str();
            let mut earlier = self.sources.iter().take(position);
            if earlier.any(|other| other.id == id) {
                return Err(Refusal::Duplicate {
                    of: Named::SourceId,
                    value: source.id.clone(),
                });
            }
        }

        // Last: within every count, this is the one rule left that the bytes alone can break.
        let found = crate::bytes_sent(self);
        if found > MANIFEST_BYTES_MAX {
            return Err(Refusal::Size { found });
        }
        Ok(())
    }
}

impl Reading {
    /// Check one poll's shape.
    ///
    /// # Errors
    ///
    /// Returns the first rule the reading breaks.
    pub fn validate(&self) -> Result<(), Refusal> {
        if !key_is_well_formed(&self.source) {
            return Err(refuse_name(Named::SourceId, &self.source));
        }
        // Past `i64::MAX` the cast wraps negative, which the lower bound then refuses. The far end
        // of `u64` is a clock that never synced, so refusing it is the point, not a near miss.
        number_within(
            Numbered::TimestampMs,
            self.ts.cast_signed(),
            0,
            TIMESTAMP_MS_MAX.cast_signed(),
        )?;
        count_within(Counted::Values, self.values.len())?;

        for (key, value) in &self.values {
            if !key_is_well_formed(key) {
                return Err(refuse_name(Named::MetricKey, key));
            }
            number_within(Numbered::Value, *value, METRIC_VALUE_MIN, METRIC_VALUE_MAX)?;
        }
        Ok(())
    }

    /// Check that this reading names a source, and metrics, the manifest declares — against its
    /// own source, never another's.
    ///
    /// # Errors
    ///
    /// Returns the source, or its first metric, the manifest does not declare.
    pub fn check_against(&self, manifest: &Manifest) -> Result<(), Refusal> {
        let undeclared = |metric: Option<&String>| Refusal::Undeclared {
            source: self.source.clone(),
            metric: metric.cloned(),
        };
        let source = manifest
            .sources
            .iter()
            .find(|source| source.id == self.source)
            .ok_or_else(|| undeclared(None))?;
        match self
            .values
            .keys()
            .find(|key| !source.metrics.iter().any(|metric| metric.key() == *key))
        {
            Some(key) => Err(undeclared(Some(key))),
            None => Ok(()),
        }
    }
}

impl Heartbeat {
    /// Check the device's account of itself.
    ///
    /// # Errors
    ///
    /// Returns the first rule the heartbeat breaks.
    pub fn validate(&self) -> Result<(), Refusal> {
        let uptime = self.uptime_seconds.cast_signed();
        number_within(
            Numbered::UptimeSeconds,
            uptime,
            0,
            UPTIME_SECONDS_MAX.cast_signed(),
        )?;
        let depth = i64::from(self.buffer_depth);
        number_within(Numbered::BufferDepth, depth, 0, i64::from(BUFFER_DEPTH_MAX))?;

        if let Some(percent) = self.battery_percent {
            let max = i64::from(BATTERY_PERCENT_MAX);
            number_within(Numbered::BatteryPercent, i64::from(percent), 0, max)?;
        }
        if let Some(percent) = self.signal_percent {
            let max = i64::from(SIGNAL_PERCENT_MAX);
            number_within(Numbered::SignalPercent, i64::from(percent), 0, max)?;
        }
        if let Some(version) = &self.firmware_version {
            text_within(Named::FirmwareVersion, version, FIRMWARE_VERSION_LENGTH_MAX)?;
        }
        Ok(())
    }
}

impl Batch {
    /// Check this batch's own shape. The manifest it names is a separate check, because a batch
    /// can be well formed and still speak of a source that was never declared.
    ///
    /// # Errors
    ///
    /// Returns the first rule the batch breaks.
    pub fn validate(&self) -> Result<(), Refusal> {
        let hash = &self.manifest_hash;
        if !hex_is_well_formed(hash, MANIFEST_HASH_HEX_LENGTH, MANIFEST_HASH_HEX_LENGTH) {
            return Err(refuse_name(Named::ManifestHash, hash));
        }
        if !hex_is_well_formed(&self.boot_id, BOOT_ID_LENGTH_MIN, BOOT_ID_LENGTH_MAX) {
            return Err(refuse_name(Named::BootId, &self.boot_id));
        }
        if !seq_is_well_formed(&self.seq) {
            return Err(refuse_name(Named::Seq, &self.seq));
        }
        count_within(Counted::Readings, self.readings.len())?;

        for reading in &self.readings {
            reading.validate()?;
        }
        self.heartbeat.validate()
    }

    /// Check that every reading names a source, and metrics, the manifest declares. The cloud
    /// rejects a batch that does not; the device should never need telling.
    ///
    /// # Errors
    ///
    /// Returns the first source or metric the manifest does not declare.
    pub fn check_against(&self, manifest: &Manifest) -> Result<(), Refusal> {
        self.readings
            .iter()
            .try_for_each(|reading| reading.check_against(manifest))
    }
}

#[cfg(test)]
mod validate_tests {
    use super::*;
    use crate::limits::{
        BOOT_ID_LENGTH_MIN, EXPONENT_MAX, KEY_LENGTH_MAX, METRIC_VALUE_MAX, METRICS_PER_SOURCE_MAX,
        READINGS_PER_BATCH_MAX, SOURCES_MAX, STATE_LABELS_MAX, TIMESTAMP_MS_MAX, TZ_LENGTH_MAX,
    };

    const HASH: &str = "d935aec39b4c492681d137f322ce5876ce1509289a3d5d759cd0b85fbf11790a";

    fn gauge(key: &str) -> Metric {
        Metric::Gauge {
            key: key.to_owned(),
            unit: Some("W".to_owned()),
            exponent: -2,
        }
    }

    fn source(id: &str, keys: &[&str]) -> Source {
        Source {
            id: id.to_owned(),
            metrics: keys.iter().map(|key| gauge(key)).collect(),
        }
    }

    fn declaring(sources: Vec<Source>) -> Manifest {
        Manifest {
            tz: "UTC".to_owned(),
            sources,
        }
    }

    fn manifest() -> Manifest {
        declaring(vec![source("source_1", &["power_w"])])
    }

    fn reading(source: &str, key: &str, value: i64) -> Reading {
        Reading {
            source: source.to_owned(),
            ts: 1_758_326_400_000,
            values: BTreeMap::from([(key.to_owned(), value)]),
        }
    }

    /// Sixteen hex digits, inside the contract's 8..=32.
    const BOOT_ID: &str = "0123456789abcdef";

    fn batch(readings: Vec<Reading>) -> Batch {
        Batch {
            manifest_hash: HASH.to_owned(),
            boot_id: BOOT_ID.to_owned(),
            seq: "1".to_owned(),
            readings,
            heartbeat: Heartbeat::new(1, 0),
        }
    }

    // Everything the contract takes, at once: a failure here means a bound is too tight.
    #[test]
    fn the_shapes_the_contract_takes_are_accepted() {
        assert_eq!(manifest().validate(), Ok(()));
        let full = batch(vec![reading("source_1", "power_w", METRIC_VALUE_MAX)]);
        assert_eq!(full.validate(), Ok(()));
        assert_eq!(full.check_against(&manifest()), Ok(()));
    }

    // Each bound, one past it. A bound loosened by one makes these pass and the suite go quiet,
    // so each names the rule it expects rather than only that something refused.
    #[test]
    fn a_count_past_its_bound_is_refused() {
        let many = declaring(
            (0..=SOURCES_MAX)
                .map(|n| source(&format!("s{n}"), &["k"]))
                .collect(),
        );
        assert_eq!(
            many.validate(),
            Err(Refusal::Count {
                of: Counted::Sources,
                found: SOURCES_MAX + 1
            })
        );

        let empty = declaring(vec![]);
        assert_eq!(
            empty.validate(),
            Err(Refusal::Count {
                of: Counted::Sources,
                found: 0
            })
        );

        let keys: Vec<String> = (0..=METRICS_PER_SOURCE_MAX)
            .map(|n| format!("k{n}"))
            .collect();
        let wide = declaring(vec![source(
            "s",
            &keys.iter().map(String::as_str).collect::<Vec<_>>(),
        )]);
        assert_eq!(
            wide.validate(),
            Err(Refusal::Count {
                of: Counted::Metrics,
                found: METRICS_PER_SOURCE_MAX + 1
            })
        );

        let long = batch(
            (0..=READINGS_PER_BATCH_MAX)
                .map(|_| reading("s", "k", 1))
                .collect(),
        );
        assert_eq!(
            long.validate(),
            Err(Refusal::Count {
                of: Counted::Readings,
                found: READINGS_PER_BATCH_MAX + 1
            })
        );
    }

    #[test]
    fn a_number_past_its_bound_is_refused() {
        let over = batch(vec![reading("source_1", "power_w", METRIC_VALUE_MAX + 1)]);
        assert_eq!(
            over.validate(),
            Err(Refusal::Number {
                of: Numbered::Value,
                found: METRIC_VALUE_MAX + 1
            })
        );

        let under = batch(vec![reading("source_1", "power_w", -METRIC_VALUE_MAX - 1)]);
        assert_eq!(
            under.validate(),
            Err(Refusal::Number {
                of: Numbered::Value,
                found: -METRIC_VALUE_MAX - 1
            })
        );

        let mut late = reading("source_1", "power_w", 1);
        late.ts = TIMESTAMP_MS_MAX + 1;
        assert!(matches!(
            batch(vec![late]).validate(),
            Err(Refusal::Number {
                of: Numbered::TimestampMs,
                ..
            })
        ));

        let steep = declaring(vec![Source {
            id: "s".to_owned(),
            metrics: vec![Metric::Gauge {
                key: "k".to_owned(),
                unit: Some("W".to_owned()),
                exponent: EXPONENT_MAX + 1,
            }],
        }]);
        assert_eq!(
            steep.validate(),
            Err(Refusal::Number {
                of: Numbered::Exponent,
                found: i64::from(EXPONENT_MAX) + 1,
            })
        );
    }

    #[test]
    fn a_unit_is_checked_only_when_present() {
        let with = |unit: Option<&str>| {
            declaring(vec![Source {
                id: "s".to_owned(),
                metrics: vec![Metric::Gauge {
                    key: "power_factor".to_owned(),
                    unit: unit.map(str::to_owned),
                    exponent: -2,
                }],
            }])
        };
        assert_eq!(with(None).validate(), Ok(()));
        assert!(matches!(
            with(Some("")).validate(),
            Err(Refusal::Name {
                of: Named::Unit,
                ..
            })
        ));
    }

    #[test]
    fn a_manifest_within_every_count_is_still_bounded_in_bytes() {
        let full = |label: &str| {
            let labels: BTreeMap<String, String> = (0..STATE_LABELS_MAX)
                .map(|code| (code.to_string(), label.to_owned()))
                .collect();
            let metrics: Vec<Metric> = (0..METRICS_PER_SOURCE_MAX)
                .map(|n| Metric::State {
                    key: format!("k{n}"),
                    state_labels: Some(labels.clone()),
                })
                .collect();
            declaring(
                (0..SOURCES_MAX)
                    .map(|n| Source {
                        id: format!("s{n}"),
                        metrics: metrics.clone(),
                    })
                    .collect(),
            )
        };

        // A control character is one byte to the label bound and six once JSON escapes it, so
        // every count can hold while the bytes sent do not.
        assert!(matches!(
            full(&"\u{1}".repeat(STATE_LABEL_LENGTH_MAX)).validate(),
            Err(Refusal::Size { .. })
        ));
        // Labels that need no escape fit: the refusal above is the bytes', not the counts'.
        assert_eq!(full(&"x".repeat(STATE_LABEL_LENGTH_MAX)).validate(), Ok(()));
    }

    // The register a driver hands back at u64's far end must not wrap into an accepted timestamp.
    #[test]
    fn a_timestamp_past_i64_does_not_wrap_into_range() {
        let mut absurd = reading("source_1", "power_w", 1);
        absurd.ts = u64::MAX;
        assert!(matches!(
            batch(vec![absurd]).validate(),
            Err(Refusal::Number {
                of: Numbered::TimestampMs,
                ..
            })
        ));
    }

    #[test]
    fn a_malformed_name_is_refused() {
        let leading = declaring(vec![source(".source", &["k"])]);
        assert!(matches!(
            leading.validate(),
            Err(Refusal::Name {
                of: Named::SourceId,
                ..
            })
        ));

        let spaced = declaring(vec![source("source 1", &["k"])]);
        assert!(matches!(
            spaced.validate(),
            Err(Refusal::Name {
                of: Named::SourceId,
                ..
            })
        ));

        let unicode = declaring(vec![source("sourceµ", &["k"])]);
        assert!(matches!(
            unicode.validate(),
            Err(Refusal::Name {
                of: Named::SourceId,
                ..
            })
        ));

        // A path segment in the cloud's read API: a colon would need escaping there.
        let coloned = declaring(vec![source("s", &["a:b"])]);
        assert!(matches!(
            coloned.validate(),
            Err(Refusal::Name {
                of: Named::MetricKey,
                ..
            })
        ));
        let unreserved = declaring(vec![source("source_1.a-b", &["_k.v-1"])]);
        assert_eq!(unreserved.validate(), Ok(()));

        let long = "k".repeat(KEY_LENGTH_MAX + 1);
        let wide = declaring(vec![source("s", &[&long])]);
        assert!(matches!(
            wide.validate(),
            Err(Refusal::Name {
                of: Named::MetricKey,
                ..
            })
        ));
    }

    // The cloud's accepted names, its longest included.
    #[test]
    fn a_zone_the_cloud_takes_is_accepted() {
        for zone in [
            "America/Sao_Paulo",
            "UTC",
            "Etc/GMT+3",
            "America/Argentina/Buenos_Aires",
            "America/Argentina/ComodRivadavia",
            "Brazil/East",
        ] {
            assert!(zone_is_known(zone), "{zone} was refused");
        }
    }

    #[test]
    fn a_zone_that_is_not_one_is_refused() {
        for zone in [
            "+03:00",
            "America/Sao Paulo",
            "Mars/Olympus_Mons",
            "",
            "America/",
            "/UTC",
            // The lookup ignores case; the contract does not.
            "america/sao_paulo",
            &format!("America/{}", "x".repeat(TZ_LENGTH_MAX)),
        ] {
            assert!(!zone_is_known(zone), "{zone:?} was accepted");
        }

        let mut zoneless = manifest();
        zoneless.tz = "+03:00".to_owned();
        assert_eq!(
            zoneless.validate(),
            Err(Refusal::Name {
                of: Named::Zone,
                value: "+03:00".to_owned(),
            })
        );
    }

    // A bundled name the pattern refused would be one the device can never declare.
    #[test]
    fn every_bundled_zone_is_accepted() {
        for zone in jiff_tzdb::available() {
            assert!(zone_is_known(zone), "{zone} was refused");
        }
    }

    // Leading zeros would spell one seq two ways, and gap detection would misread the sequence.
    #[test]
    fn a_seq_that_is_not_canonical_decimal_is_refused() {
        for spelling in ["01", "", "-1", "1.0", "18446744073709551616", " 1"] {
            let mut odd = batch(vec![reading("source_1", "power_w", 1)]);
            odd.seq = spelling.to_owned();
            assert!(
                matches!(odd.validate(), Err(Refusal::Name { of: Named::Seq, .. })),
                "{spelling:?} was accepted"
            );
        }

        let mut zero = batch(vec![reading("source_1", "power_w", 1)]);
        zero.seq = "0".to_owned();
        assert_eq!(zero.validate(), Ok(()));

        let mut ceiling = batch(vec![reading("source_1", "power_w", 1)]);
        ceiling.seq = u64::MAX.to_string();
        assert_eq!(ceiling.validate(), Ok(()));
    }

    #[test]
    fn a_manifest_hash_that_is_not_lowercase_sha256_is_refused() {
        for spelling in [&HASH.to_uppercase(), "abc", &format!("{HASH}0")] {
            let mut odd = batch(vec![reading("source_1", "power_w", 1)]);
            odd.manifest_hash = (*spelling).to_string();
            assert!(
                matches!(
                    odd.validate(),
                    Err(Refusal::Name {
                        of: Named::ManifestHash,
                        ..
                    })
                ),
                "{spelling:?} was accepted"
            );
        }
    }

    #[test]
    fn a_repeated_id_or_key_is_refused() {
        let twice = declaring(vec![source("s", &["k"]), source("s", &["k"])]);
        assert_eq!(
            twice.validate(),
            Err(Refusal::Duplicate {
                of: Named::SourceId,
                value: "s".to_owned()
            })
        );

        let both = declaring(vec![source("s", &["k", "k"])]);
        assert_eq!(
            both.validate(),
            Err(Refusal::Duplicate {
                of: Named::MetricKey,
                value: "k".to_owned()
            })
        );
    }

    // What the cloud would answer with a 4xx. The device should never need telling.
    #[test]
    fn a_reading_the_manifest_does_not_declare_is_refused() {
        let stranger = batch(vec![reading("source_2", "power_w", 1)]);
        assert_eq!(
            stranger.check_against(&manifest()),
            Err(Refusal::Undeclared {
                source: "source_2".to_owned(),
                metric: None
            })
        );

        let undeclared = batch(vec![reading("source_1", "voltage_v", 1)]);
        assert_eq!(
            undeclared.check_against(&manifest()),
            Err(Refusal::Undeclared {
                source: "source_1".to_owned(),
                metric: Some("voltage_v".to_owned()),
            })
        );
    }

    // One reading's keys are checked against its own source, never against another's.
    #[test]
    fn each_reading_is_checked_against_its_own_source() {
        let two = declaring(vec![
            source("source_1", &["power_w"]),
            source("source_2", &["voltage_v"]),
        ]);
        let mixed = batch(vec![
            reading("source_1", "power_w", 1),
            reading("source_2", "voltage_v", 2),
        ]);
        assert_eq!(mixed.check_against(&two), Ok(()));
    }

    #[test]
    fn a_batch_without_a_well_formed_boot_id_is_refused() {
        // Half of what identifies a batch, so a malformed one costs more than a bad field: it
        // makes two boots look like one, and a loss in either hides behind the other.
        for bad in [
            "0".repeat(BOOT_ID_LENGTH_MIN - 1),
            "0".repeat(BOOT_ID_LENGTH_MAX + 1),
            "0123456789abcdeG".to_owned(),
            String::new(),
        ] {
            let mut wrong = batch(vec![reading("source_1", "power_w", 1)]);
            wrong.boot_id = bad.clone();
            assert!(
                matches!(
                    wrong.validate(),
                    Err(Refusal::Name {
                        of: Named::BootId,
                        ..
                    })
                ),
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn a_heartbeat_past_its_bounds_is_refused() {
        let mut charged = batch(vec![reading("source_1", "power_w", 1)]);
        charged.heartbeat = Heartbeat {
            battery_percent: Some(101),
            ..Heartbeat::new(1, 1)
        };
        assert_eq!(
            charged.validate(),
            Err(Refusal::Number {
                of: Numbered::BatteryPercent,
                found: 101
            })
        );
    }
}
