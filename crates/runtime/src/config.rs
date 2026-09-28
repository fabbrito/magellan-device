//! What the device is told at startup, and what it refuses to start without.
//!
//! Two sources, deliberately apart. The file holds settings that describe the *deployment* and
//! are safe to commit; the environment holds everything that identifies one *installation* — the
//! device token, the site's coordinates and zone, a logger's serial and address. `AGENTS.md` keeps
//! credentials and home-network details out of git, and a schema that cannot express them is a
//! stronger guarantee than remembering not to write them down.
//!
//! Unknown keys are rejected, so a typo fails at startup rather than silently taking a default. A
//! timing left out takes its default, so a file written before it was a setting still reads.

use std::collections::BTreeSet;
use std::env;
use std::fmt;
use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use contract::limits::SOURCES_MAX;
use contract::{key_is_well_formed, zone_is_known};
use jiff::SignedDuration;
use serde::Deserialize;

use crate::Cadence;
use crate::sun::{LATITUDE_DEG_MAX, Site};

/// Past three hours, a margin polls a dark source for most of the night.
const MARGIN_MINUTES_MAX: u32 = 180;
/// Defaults for the timings a file may leave out, in the unit its key names.
const REQUEST_TIMEOUT_S: u64 = 20;
const PACE_S: u64 = 1;
const BACKOFF_FIRST_S: u64 = 5;
const BACKOFF_CEILING_S: u64 = 300;
const RECHECK_MIN: u64 = 15;
const FLUSH_S: u64 = 60;
/// Environment variables carrying the per-installation identity.
const DEVICE_ID_VAR: &str = "MAGELLAN_DEVICE_ID";
const TOKEN_VAR: &str = "MAGELLAN_TOKEN";
const LATITUDE_VAR: &str = "MAGELLAN_LATITUDE";
const LONGITUDE_VAR: &str = "MAGELLAN_LONGITUDE";
const TZ_VAR: &str = "MAGELLAN_TZ";

/// A device token.
///
/// Its `Debug` is redacted. A token reaching the journal is a disclosure, and a journal is copied,
/// shipped and pasted into issues — the one place a credential must not be able to arrive by
/// accident.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// The token itself. Every call site is one that puts it on the wire.
    #[must_use]
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
impl Token {
    pub(crate) fn fixture(token: &str) -> Self {
        Self(token.to_owned())
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(redacted)")
    }
}

/// Everything the runtime needs to start.
#[derive(Debug)]
pub struct Config {
    pub device_id: String,
    pub token: Token,
    pub endpoint: String,
    /// Longest one request to the cloud may take. A source and the cloud are different networks,
    /// so this moves for its own reasons and is not a driver's read limit under another name.
    pub request_timeout: Duration,
    pub(crate) cadence: Cadence,
    pub buffer: NonZeroUsize,
    pub(crate) margins: Margins,
    pub(crate) site: Site,
    /// The IANA zone the manifest declares.
    pub zone: String,
    pub sources: Vec<SourceConfig>,
}

/// How far either side of daylight the device keeps polling.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Margins {
    pub(crate) before_sunrise: SignedDuration,
    pub(crate) after_sunset: SignedDuration,
}

/// One source to construct at boot.
#[derive(Debug)]
pub struct SourceConfig {
    /// Becomes `Source::id` in the manifest, so the contract's pattern binds it.
    pub id: String,
    /// Which driver reads it.
    pub driver: String,
    /// The driver's own settings, as written. The driver reads and refuses them, not this.
    pub settings: toml::Table,
    /// `MAGELLAN_SOURCE_<ID>_`, before the driver's own key.
    var_prefix: String,
}

impl SourceConfig {
    /// This source's own environment variable `key` — `SERIAL` is `MAGELLAN_SOURCE_<ID>_SERIAL`.
    /// Empty is unset. What a driver identifies one installation by lives here, never in the file.
    #[must_use]
    pub fn var(&self, key: &str) -> Option<String> {
        env::var(format!("{}{key}", self.var_prefix))
            .ok()
            .filter(|value| !value.is_empty())
    }

    /// What every one of this source's variables starts with, for a message naming them.
    #[must_use]
    pub fn var_prefix(&self) -> &str {
        &self.var_prefix
    }
}

/// The file as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    cloud: RawCloud,
    poll: RawPoll,
    #[serde(default)]
    drain: RawDrain,
    buffer: RawBuffer,
    window: RawWindow,
    source: Vec<RawSource>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCloud {
    endpoint: String,
    #[serde(default = "request_timeout_s")]
    request_timeout_s: u64,
}

const fn request_timeout_s() -> u64 {
    REQUEST_TIMEOUT_S
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawDrain {
    #[serde(rename = "pace_s")]
    pace: u64,
    #[serde(rename = "backoff_first_s")]
    backoff_first: u64,
    #[serde(rename = "backoff_ceiling_s")]
    backoff_ceiling: u64,
    #[serde(rename = "flush_s")]
    flush: u64,
}

impl Default for RawDrain {
    fn default() -> Self {
        Self {
            pace: PACE_S,
            backoff_first: BACKOFF_FIRST_S,
            backoff_ceiling: BACKOFF_CEILING_S,
            flush: FLUSH_S,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPoll {
    sweep_period_s: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBuffer {
    batches_max: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWindow {
    #[serde(rename = "before_sunrise_min")]
    before_sunrise: u32,
    #[serde(rename = "after_sunset_min")]
    after_sunset: u32,
    #[serde(rename = "recheck_min", default = "recheck_min")]
    recheck: u64,
}

const fn recheck_min() -> u64 {
    RECHECK_MIN
}

#[derive(Deserialize)]
struct RawSource {
    id: String,
    driver: String,
    #[serde(flatten)]
    settings: toml::Table,
}

impl Config {
    /// Read the file at `path` and the environment around it.
    ///
    /// # Errors
    ///
    /// If the file cannot be read or parsed, if it carries a key the schema does not know, if a
    /// bound is broken, or if any variable naming this installation is missing.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    /// The same, from text already in hand.
    ///
    /// # Errors
    ///
    /// As [`Config::load`], less the reading.
    pub fn parse(text: &str) -> Result<Self> {
        let raw: Raw = toml::from_str(text).context("parsing the configuration")?;
        check_endpoint(&raw.cloud.endpoint)?;
        let cadence = read_cadence(&raw)?;
        let request_timeout = seconds("cloud.request_timeout_s", raw.cloud.request_timeout_s)?;
        for (name, minutes) in [
            ("before_sunrise_min", raw.window.before_sunrise),
            ("after_sunset_min", raw.window.after_sunset),
        ] {
            ensure!(
                minutes <= MARGIN_MINUTES_MAX,
                "window.{name} is {minutes}, past the {MARGIN_MINUTES_MAX} minute ceiling"
            );
        }
        let buffer = NonZeroUsize::new(raw.buffer.batches_max)
            .context("buffer.batches_max is 0, so every reading is dropped as it is made")?;
        let site = read_site()?;
        let zone = read_var(TZ_VAR)?;
        ensure!(
            zone_is_known(&zone),
            "{TZ_VAR} is {zone:?}, not an IANA zone spelled as the tz database spells it"
        );

        ensure!(!raw.source.is_empty(), "no [[source]] to read");
        ensure!(
            raw.source.len() <= SOURCES_MAX,
            "{} sources, past the contract's {SOURCES_MAX}",
            raw.source.len()
        );
        let mut seen = BTreeSet::new();
        let mut sources = Vec::with_capacity(raw.source.len());
        for source in raw.source {
            ensure!(
                key_is_well_formed(&source.id),
                "source id {:?} is not a shape the contract accepts",
                source.id
            );
            ensure!(
                seen.insert(source.id.clone()),
                "two sources share the id {:?}",
                source.id
            );
            sources.push(read_source(source));
        }

        Ok(Self {
            device_id: read_var(DEVICE_ID_VAR)?,
            token: Token(read_var(TOKEN_VAR)?),
            endpoint: raw.cloud.endpoint,
            request_timeout,
            cadence,
            buffer,
            margins: Margins {
                before_sunrise: SignedDuration::from_mins(i64::from(raw.window.before_sunrise)),
                after_sunset: SignedDuration::from_mins(i64::from(raw.window.after_sunset)),
            },
            site,
            zone,
            sources,
        })
    }
}

/// Every interval the runtime keeps. None may be zero: a zero sweep is no schedule, a zero backoff
/// is asking again at once, and a zero pace is the burst it exists to prevent.
fn read_cadence(raw: &Raw) -> Result<Cadence> {
    let backoff_first = seconds("drain.backoff_first_s", raw.drain.backoff_first)?;
    let backoff_ceiling = seconds("drain.backoff_ceiling_s", raw.drain.backoff_ceiling)?;
    ensure!(
        backoff_first <= backoff_ceiling,
        "drain.backoff_first_s is past drain.backoff_ceiling_s, so the backoff starts above its cap"
    );
    Ok(Cadence {
        sweep: seconds("poll.sweep_period_s", raw.poll.sweep_period_s)?,
        backoff_first,
        backoff_ceiling,
        drain_pace: seconds("drain.pace_s", raw.drain.pace)?,
        recheck: seconds("window.recheck_min", raw.window.recheck.saturating_mul(60))?,
        flush: seconds("drain.flush_s", raw.drain.flush)?,
    })
}

/// A timing that must not be zero, named by its key.
fn seconds(key: &str, seconds: u64) -> Result<Duration> {
    ensure!(seconds > 0, "{key} is 0");
    Ok(Duration::from_secs(seconds))
}

/// A token must not cross the network in the clear.
fn check_endpoint(endpoint: &str) -> Result<()> {
    if endpoint.starts_with("https://") {
        return Ok(());
    }
    // Loopback is the fake cloud the tests run against; nothing leaves the machine.
    let loopback = endpoint.starts_with("http://127.0.0.1")
        || endpoint.starts_with("http://localhost")
        || endpoint.starts_with("http://[::1]");
    ensure!(
        loopback,
        "cloud.endpoint {endpoint:?} is not https, and every request carries the device token"
    );
    Ok(())
}

/// The site's coordinates, from the environment: where the sun rises is where the installation is,
/// and that is a home address by another name.
fn read_site() -> Result<Site> {
    let latitude = read_var(LATITUDE_VAR)?
        .parse::<f64>()
        .with_context(|| format!("{LATITUDE_VAR} is not a number"))?;
    let longitude = read_var(LONGITUDE_VAR)?
        .parse::<f64>()
        .with_context(|| format!("{LONGITUDE_VAR} is not a number"))?;
    ensure!(
        latitude.abs() <= LATITUDE_DEG_MAX,
        "{LATITUDE_VAR} is {latitude}, past the {LATITUDE_DEG_MAX} degrees where the sun still \
         rises and sets every day"
    );
    ensure!(
        longitude.abs() <= 180.0,
        "{LONGITUDE_VAR} is {longitude}, which is not a longitude"
    );
    Ok(Site {
        latitude,
        longitude,
    })
}

/// A source as written, and where its own environment is. Which variables a source needs is its
/// driver's to say.
fn read_source(raw: RawSource) -> SourceConfig {
    // Validated first, so whatever is not alphanumeric is contract punctuation, and a variable
    // name admits none of it.
    let upper: String = raw
        .id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    SourceConfig {
        id: raw.id,
        driver: raw.driver,
        settings: raw.settings,
        var_prefix: format!("MAGELLAN_SOURCE_{upper}_"),
    }
}

fn read_var(name: &str) -> Result<String> {
    match env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        Ok(_) => bail!("{name} is set but empty"),
        Err(_) => bail!("{name} is not set; it names this installation and is not in the file"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [cloud]
        endpoint = "https://cloud.example/v1"

        [poll]
        sweep_period_s = 300

        [buffer]
        batches_max = 64

        [window]
        before_sunrise_min = 30
        after_sunset_min = 30

        [[source]]
        id = "inverter"
        driver = "sofar"
        profile = "sofar-g3"
        port = 8899
    "#;

    /// Set the variables that name an installation.
    ///
    /// Sound because nextest gives every test its own process, so no other thread is reading the
    /// environment while this writes it.
    fn set_env() {
        for (name, value) in [
            (DEVICE_ID_VAR, "device_1"),
            (TOKEN_VAR, "s3cret"),
            (LATITUDE_VAR, "-23.55"),
            (LONGITUDE_VAR, "-46.63"),
            (TZ_VAR, "America/Sao_Paulo"),
            ("MAGELLAN_SOURCE_INVERTER_SERIAL", "3735928559"),
        ] {
            unsafe { env::set_var(name, value) };
        }
    }

    fn parse(text: &str) -> Result<Config> {
        set_env();
        Config::parse(text)
    }

    #[test]
    fn a_minimal_config_parses() {
        let config = parse(MINIMAL).expect("parses");
        assert_eq!(config.device_id, "device_1");
        assert_eq!(config.zone, "America/Sao_Paulo");
        assert_eq!(config.cadence.sweep, Duration::from_secs(300));
        assert_eq!(config.buffer.get(), 64);
        assert_eq!(config.sources.len(), 1);
        let source = &config.sources[0];
        assert_eq!(
            (source.id.as_str(), source.driver.as_str()),
            ("inverter", "sofar")
        );
        // What the schema does not name stays as the driver's to read.
        assert_eq!(source.settings["port"].as_integer(), Some(8899));
    }

    #[test]
    fn a_timing_left_out_takes_its_default() {
        // A file written before the timings were settings still reads, and runs as it did.
        let config = parse(MINIMAL).expect("parses");
        assert_eq!(config.request_timeout, Duration::from_secs(20));
        let cadence = config.cadence;
        assert_eq!(cadence.drain_pace, Duration::from_secs(1));
        assert_eq!(cadence.backoff_first, Duration::from_secs(5));
        assert_eq!(cadence.backoff_ceiling, Duration::from_mins(5));
        assert_eq!(cadence.recheck, Duration::from_mins(15));
        assert_eq!(cadence.flush, Duration::from_mins(1));
    }

    #[test]
    fn the_example_parses_and_shows_the_defaults() {
        // Its comment says a timing left out takes the value shown there; this holds it to that.
        let example = parse(include_str!("../../../config.example.toml")).expect("parses");
        let defaults = parse(MINIMAL).expect("parses");
        assert_eq!(example.request_timeout, defaults.request_timeout);
        assert_eq!(
            format!("{:?}", example.cadence),
            format!("{:?}", defaults.cadence)
        );
    }

    #[test]
    fn a_timing_written_down_is_the_one_kept() {
        let text = MINIMAL
            .replace(
                "endpoint = \"https://cloud.example/v1\"",
                "endpoint = \"https://cloud.example/v1\"\nrequest_timeout_s = 7",
            )
            .replace(
                "after_sunset_min = 30",
                "after_sunset_min = 30\nrecheck_min = 3",
            )
            + "\n[drain]\npace_s = 2\nbackoff_first_s = 4\nbackoff_ceiling_s = 60\n";
        let config = parse(&text).expect("parses");
        assert_eq!(config.request_timeout, Duration::from_secs(7));
        let cadence = config.cadence;
        assert_eq!(cadence.drain_pace, Duration::from_secs(2));
        assert_eq!(cadence.backoff_first, Duration::from_secs(4));
        assert_eq!(cadence.backoff_ceiling, Duration::from_mins(1));
        assert_eq!(cadence.recheck, Duration::from_mins(3));
    }

    #[test]
    fn a_zero_timing_is_refused() {
        // A zero pace is the burst the pace exists to prevent; a zero backoff asks again at once.
        for (key, section) in [
            ("pace_s", "drain"),
            ("backoff_first_s", "drain"),
            ("flush_s", "drain"),
            ("recheck_min", "window"),
        ] {
            let text = if section == "drain" {
                format!("{MINIMAL}\n[drain]\n{key} = 0\n")
            } else {
                MINIMAL.replace(
                    "after_sunset_min = 30",
                    &format!("after_sunset_min = 30\n{key} = 0"),
                )
            };
            let refused = parse(&text).expect_err(key).to_string();
            assert!(refused.contains(key), "{refused}");
        }
    }

    #[test]
    fn a_backoff_that_starts_above_its_ceiling_is_refused() {
        let text = format!("{MINIMAL}\n[drain]\nbackoff_first_s = 600\n");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn an_unknown_drain_key_is_rejected() {
        let text = format!("{MINIMAL}\n[drain]\npace_ms = 500\n");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        // A typo that silently took a default would be found in the archive, months later.
        let text = MINIMAL.replace(
            "sweep_period_s = 300",
            "sweep_period_s = 300\nsweep_gap_s = 5",
        );
        assert!(parse(&text).is_err());
    }

    #[test]
    fn the_token_never_shows_up_in_a_debug_line() {
        // The journal is copied, shipped and pasted into issues. A credential must not be able to
        // arrive there by accident.
        let config = parse(MINIMAL).expect("parses");
        assert_eq!(format!("{:?}", config.token), "Token(redacted)");
        assert!(!format!("{config:?}").contains("s3cret"));
        assert_eq!(config.token.reveal(), "s3cret");
    }

    #[test]
    fn a_cleartext_endpoint_is_refused_unless_it_is_loopback() {
        let plain = MINIMAL.replace("https://cloud.example/v1", "http://cloud.example/v1");
        let err = parse(&plain).expect_err("http must be refused");
        assert!(err.to_string().contains("device token"), "{err}");

        // The fake cloud the tests reach for never leaves the machine.
        let local = MINIMAL.replace("https://cloud.example/v1", "http://127.0.0.1:8080");
        assert!(parse(&local).is_ok());
    }

    #[test]
    fn a_missing_variable_stops_the_device_at_startup() {
        set_env();
        unsafe { env::remove_var(TOKEN_VAR) };
        let err = Config::parse(MINIMAL).expect_err("no token, no start");
        assert!(err.to_string().contains(TOKEN_VAR), "{err}");
    }

    #[test]
    fn an_empty_variable_is_not_a_value() {
        set_env();
        unsafe { env::set_var(TOKEN_VAR, "") };
        assert!(Config::parse(MINIMAL).is_err());
    }

    #[test]
    fn two_sources_cannot_share_an_id() {
        let text = format!("{MINIMAL}\n[[source]]\nid = \"inverter\"\ndriver = \"sofar\"\n");
        let err = parse(&text).expect_err("duplicate ids must be refused");
        assert!(err.to_string().contains("share the id"), "{err}");
    }

    #[test]
    fn a_source_id_the_contract_would_reject_is_refused_here() {
        // Better at startup than at the first upload, where it costs a round trip and a 4xx.
        let text = MINIMAL.replace(r#"id = "inverter""#, r#"id = "inverter/1""#);
        let err = parse(&text).expect_err("a bad id must be refused");
        assert!(err.to_string().contains("contract accepts"), "{err}");
    }

    #[test]
    fn a_buffer_that_holds_nothing_is_refused() {
        let text = MINIMAL.replace("batches_max = 64", "batches_max = 0");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn a_site_past_the_latitude_bound_is_refused() {
        set_env();
        unsafe { env::set_var(LATITUDE_VAR, "70.0") };
        let err = Config::parse(MINIMAL).expect_err("past the bound");
        assert!(err.to_string().contains("rises and sets"), "{err}");
    }

    #[test]
    fn a_zone_the_contract_would_reject_is_refused_here() {
        set_env();
        unsafe { env::set_var(TZ_VAR, "-03:00") };
        let err = Config::parse(MINIMAL).expect_err("an offset is not a zone");
        assert!(err.to_string().contains(TZ_VAR), "{err}");
    }

    #[test]
    fn a_margin_past_the_ceiling_is_refused() {
        let text = MINIMAL.replace("after_sunset_min = 30", "after_sunset_min = 240");
        let err = parse(&text).expect_err("past the ceiling");
        assert!(err.to_string().contains("ceiling"), "{err}");
    }

    #[test]
    fn a_source_reads_its_own_variables_by_key() {
        // The driver knows `SERIAL`; which variable that is stays here. Empty is unset.
        set_env();
        unsafe { env::set_var("MAGELLAN_SOURCE_INVERTER_HOST", "") };
        let config = Config::parse(MINIMAL).expect("parses");
        let source = &config.sources[0];
        assert_eq!(source.var("SERIAL").as_deref(), Some("3735928559"));
        assert_eq!(source.var("HOST"), None);
        assert_eq!(source.var_prefix(), "MAGELLAN_SOURCE_INVERTER_");
    }

    #[test]
    fn a_source_id_with_punctuation_names_its_variables_with_underscores() {
        let text = MINIMAL.replace("id = \"inverter\"", "id = \"roof-inverter.2\"");
        let config = parse(&text).expect("parses");
        assert_eq!(
            config.sources[0].var_prefix(),
            "MAGELLAN_SOURCE_ROOF_INVERTER_2_"
        );
    }

    #[test]
    fn a_config_with_no_source_reads_nothing_and_is_refused() {
        let text = MINIMAL.replace(
            "[[source]]\n        id = \"inverter\"\n        driver = \"sofar\"\n        profile = \"sofar-g3\"\n        port = 8899",
            "",
        );
        assert!(parse(&text).is_err());
    }
}
