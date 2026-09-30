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

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use contract::limits::SOURCES_MAX;
use contract::{key_is_well_formed, zone_is_known};
use jiff::SignedDuration;
use serde::Deserialize;

use crate::Cadence;
use crate::sun::{LATITUDE_DEG_MAX, Site};
use crate::window::Sun;

/// Past three hours, a margin polls a dark source for most of the night.
const MARGIN_MINUTES_MAX: u32 = 180;
/// Defaults for the timings a file may leave out, in the unit its key names.
const REQUEST_TIMEOUT_S: u64 = 20;
const PACE_S: u64 = 1;
const BACKOFF_FIRST_S: u64 = 5;
const BACKOFF_CEILING_S: u64 = 300;
const RECHECK_MIN: u64 = 15;
const HEARTBEAT_PERIOD_S: u64 = 3600;
/// A week of sweeps at the default five minutes, around the clock: a bound on flash, not a
/// schedule. At a few KB a batch, single-digit MB.
const BATCHES_MAX: usize = 7 * 24 * 12;
/// Environment variables carrying the per-installation identity.
const DEVICE_ID_VAR: &str = "MAGELLAN_DEVICE_ID";
const TOKEN_VAR: &str = "MAGELLAN_TOKEN";
const LATITUDE_VAR: &str = "MAGELLAN_LATITUDE";
const LONGITUDE_VAR: &str = "MAGELLAN_LONGITUDE";
const TZ_VAR: &str = "MAGELLAN_TZ";
/// What every variable the device reads starts with; the rest of the environment is not its own.
const VAR_PREFIX: &str = "MAGELLAN_";

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
    /// Where the buffer is written through to. The service manager makes it.
    pub buffer_dir: PathBuf,
    /// The IANA zone the manifest declares.
    pub zone: String,
    pub sources: Vec<SourceConfig>,
}

/// One source to construct at boot.
pub struct SourceConfig {
    /// Becomes `Source::id` in the manifest, so the contract's pattern binds it.
    pub id: String,
    /// Which driver reads it.
    pub driver: String,
    /// The driver's own settings, as written. The driver reads and refuses them, not this.
    pub settings: toml::Table,
    /// When it is worth polling. `None` is always.
    pub(crate) window: Option<Sun>,
    /// `MAGELLAN_SOURCE_<ID>_`, before the driver's own key.
    var_prefix: String,
    /// This source's variables by the driver's key, prefix stripped, empty ones left out.
    vars: BTreeMap<String, String>,
}

impl SourceConfig {
    /// This source's own environment variable `key` — `SERIAL` is `MAGELLAN_SOURCE_<ID>_SERIAL`.
    /// Empty is unset. What a driver identifies one installation by lives here, never in the file.
    #[must_use]
    pub fn var(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }

    /// What every one of this source's variables starts with, for a message naming them.
    #[must_use]
    pub fn var_prefix(&self) -> &str {
        &self.var_prefix
    }
}

/// Names its variables, never their values: a serial or an address names one installation, and a
/// journal is copied, shipped and pasted into issues.
impl fmt::Debug for SourceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceConfig")
            .field("id", &self.id)
            .field("driver", &self.driver)
            .field("settings", &self.settings)
            .field("window", &self.window.is_some())
            .field("var_prefix", &self.var_prefix)
            .field("vars", &self.vars.keys().collect::<Vec<_>>())
            .finish()
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
    #[serde(default)]
    heartbeat: RawHeartbeat,
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
}

impl Default for RawDrain {
    fn default() -> Self {
        Self {
            pace: PACE_S,
            backoff_first: BACKOFF_FIRST_S,
            backoff_ceiling: BACKOFF_CEILING_S,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawHeartbeat {
    #[serde(rename = "period_s")]
    period: u64,
}

impl Default for RawHeartbeat {
    fn default() -> Self {
        Self {
            period: HEARTBEAT_PERIOD_S,
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
    dir: PathBuf,
    #[serde(default = "batches_max")]
    batches_max: usize,
}

const fn batches_max() -> usize {
    BATCHES_MAX
}

/// A source's window, as written inside its `[[source]]`. One kind today; the tag leaves room.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RawWindow {
    /// Daylight at the site, with margins: a source that runs on its panels.
    Sun {
        #[serde(rename = "before_sunrise_min")]
        before_sunrise: u32,
        #[serde(rename = "after_sunset_min")]
        after_sunset: u32,
        #[serde(rename = "recheck_min", default = "recheck_min")]
        recheck: u64,
    },
}

/// The runtime's own key inside a `[[source]]`, taken out before the driver sees the rest.
const WINDOW_KEY: &str = "window";

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
        // Read once, here. Writing the environment is unsound once a thread runs, so nothing
        // after startup reads it either, and a test hands `parse` a map instead.
        let vars = env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
            .filter(|(name, _)| name.starts_with(VAR_PREFIX))
            .collect();
        Self::parse(&text, &vars)
    }

    /// The same, from text and variables already in hand.
    ///
    /// # Errors
    ///
    /// As [`Config::load`], less the reading.
    pub(crate) fn parse(text: &str, vars: &BTreeMap<String, String>) -> Result<Self> {
        let raw: Raw = toml::from_str(text).context("parsing the configuration")?;
        check_endpoint(&raw.cloud.endpoint)?;
        let cadence = read_cadence(&raw)?;
        let request_timeout = seconds("cloud.request_timeout_s", raw.cloud.request_timeout_s)?;
        let buffer = NonZeroUsize::new(raw.buffer.batches_max)
            .context("buffer.batches_max is 0, so every reading is dropped as it is made")?;
        let zone = read_var(vars, TZ_VAR)?;
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
        // Read once, and only when a window needs it: a device with no sun window names no site.
        let mut site: Option<Site> = None;
        for mut source in raw.source {
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
            let window = match source.settings.remove(WINDOW_KEY) {
                None => None,
                Some(written) => {
                    let site = match site {
                        Some(site) => site,
                        None => *site.insert(read_site(vars)?),
                    };
                    Some(read_window(&source.id, written, site)?)
                }
            };
            sources.push(read_source(source, window, vars));
        }

        Ok(Self {
            device_id: read_var(vars, DEVICE_ID_VAR)?,
            token: Token(read_var(vars, TOKEN_VAR)?),
            endpoint: raw.cloud.endpoint,
            request_timeout,
            cadence,
            buffer,
            buffer_dir: raw.buffer.dir,
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
        heartbeat: seconds("heartbeat.period_s", raw.heartbeat.period)?,
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
fn read_site(vars: &BTreeMap<String, String>) -> Result<Site> {
    let latitude = read_var(vars, LATITUDE_VAR)?
        .parse::<f64>()
        .with_context(|| format!("{LATITUDE_VAR} is not a number"))?;
    let longitude = read_var(vars, LONGITUDE_VAR)?
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

/// A source's window as written, named by the source it belongs to.
fn read_window(id: &str, written: toml::Value, site: Site) -> Result<Sun> {
    let raw: RawWindow = written
        .try_into()
        .with_context(|| format!("source {id:?}: reading its window"))?;
    let RawWindow::Sun {
        before_sunrise,
        after_sunset,
        recheck,
    } = raw;
    for (name, minutes) in [
        ("before_sunrise_min", before_sunrise),
        ("after_sunset_min", after_sunset),
    ] {
        ensure!(
            minutes <= MARGIN_MINUTES_MAX,
            "source {id:?}: window.{name} is {minutes}, past the {MARGIN_MINUTES_MAX} minute \
             ceiling"
        );
    }
    Ok(Sun {
        site,
        before_sunrise: SignedDuration::from_mins(i64::from(before_sunrise)),
        after_sunset: SignedDuration::from_mins(i64::from(after_sunset)),
        recheck: seconds(
            &format!("source {id:?}: window.recheck_min"),
            recheck.saturating_mul(60),
        )?,
    })
}

/// A source as written, and where its own environment is. Which variables a source needs is its
/// driver's to say.
fn read_source(
    raw: RawSource,
    window: Option<Sun>,
    vars: &BTreeMap<String, String>,
) -> SourceConfig {
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
    let var_prefix = format!("{VAR_PREFIX}SOURCE_{upper}_");
    let vars = vars
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .filter_map(|(name, value)| {
            let key = name.strip_prefix(&var_prefix)?;
            Some((key.to_owned(), value.clone()))
        })
        .collect();
    SourceConfig {
        id: raw.id,
        driver: raw.driver,
        settings: raw.settings,
        window,
        var_prefix,
        vars,
    }
}

fn read_var(vars: &BTreeMap<String, String>, name: &str) -> Result<String> {
    match vars.get(name) {
        Some(value) if !value.is_empty() => Ok(value.clone()),
        Some(_) => bail!("{name} is set but empty"),
        None => bail!("{name} is not set; it names this installation and is not in the file"),
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
        dir = "/var/lib/magellan"

        [[source]]
        id = "inverter"
        driver = "sofar"
        window = { kind = "sun", before_sunrise_min = 30, after_sunset_min = 30 }
        profile = "sofar-g3"
        port = 8899
    "#;

    /// The variables that name an installation.
    fn vars() -> BTreeMap<String, String> {
        [
            (DEVICE_ID_VAR, "device_1"),
            (TOKEN_VAR, "s3cret"),
            (LATITUDE_VAR, "-23.55"),
            (LONGITUDE_VAR, "-46.63"),
            (TZ_VAR, "America/Sao_Paulo"),
            ("MAGELLAN_SOURCE_INVERTER_SERIAL", "3735928559"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
    }

    /// The installation's variables with `name` set to `value`.
    fn with(name: &str, value: &str) -> BTreeMap<String, String> {
        let mut vars = vars();
        vars.insert(name.to_owned(), value.to_owned());
        vars
    }

    fn parse(text: &str) -> Result<Config> {
        Config::parse(text, &vars())
    }

    #[test]
    fn a_minimal_config_parses() {
        let config = parse(MINIMAL).expect("parses");
        assert_eq!(config.device_id, "device_1");
        assert_eq!(config.zone, "America/Sao_Paulo");
        assert_eq!(config.cadence.sweep, Duration::from_secs(300));
        assert_eq!(config.buffer.get(), 2016, "a week of sweeps");
        assert_eq!(config.buffer_dir, Path::new("/var/lib/magellan"));
        assert_eq!(config.sources.len(), 1);
        let source = &config.sources[0];
        assert_eq!(
            (source.id.as_str(), source.driver.as_str()),
            ("inverter", "sofar")
        );
        // What the schema does not name stays as the driver's to read; the window is not the
        // driver's, and would be refused as a key it does not know.
        assert_eq!(source.settings["port"].as_integer(), Some(8899));
        assert!(!source.settings.contains_key("window"));
        let window = source.window.expect("a sun window");
        assert_eq!(window.before_sunrise, SignedDuration::from_mins(30));
        assert_eq!(window.recheck, Duration::from_mins(15), "the default");
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
        assert_eq!(cadence.heartbeat, Duration::from_hours(1));
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
                "after_sunset_min = 30 }",
                "after_sunset_min = 30, recheck_min = 3 }",
            )
            + "\n[drain]\npace_s = 2\nbackoff_first_s = 4\nbackoff_ceiling_s = 60\n";
        let config = parse(&text).expect("parses");
        assert_eq!(config.request_timeout, Duration::from_secs(7));
        let cadence = config.cadence;
        assert_eq!(cadence.drain_pace, Duration::from_secs(2));
        assert_eq!(cadence.backoff_first, Duration::from_secs(4));
        assert_eq!(cadence.backoff_ceiling, Duration::from_mins(1));
        let window = config.sources[0].window.expect("a sun window");
        assert_eq!(window.recheck, Duration::from_mins(3));
    }

    #[test]
    fn a_zero_timing_is_refused() {
        // A zero pace is the burst the pace exists to prevent; a zero backoff asks again at once.
        for (key, section) in [
            ("pace_s", "drain"),
            ("backoff_first_s", "drain"),
            ("recheck_min", "window"),
            ("period_s", "heartbeat"),
        ] {
            let text = if section == "window" {
                MINIMAL.replace(
                    "after_sunset_min = 30 }",
                    &format!("after_sunset_min = 30, {key} = 0 }}"),
                )
            } else {
                format!("{MINIMAL}\n[{section}]\n{key} = 0\n")
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
        // `flush_s` among them: a stop loses nothing now, and a file setting it predates that.
        for key in ["pace_ms = 500", "flush_s = 60"] {
            let text = format!("{MINIMAL}\n[drain]\n{key}\n");
            assert!(parse(&text).is_err(), "{key}");
        }
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
    fn a_sources_variables_never_show_up_in_a_debug_line() {
        // A serial and a home address name one installation, as the token names one device.
        let vars = with("MAGELLAN_SOURCE_INVERTER_HOST", "192.0.2.7");
        let config = Config::parse(MINIMAL, &vars).expect("parses");
        let line = format!("{config:?}");
        assert!(!line.contains("3735928559"), "{line}");
        assert!(!line.contains("192.0.2.7"), "{line}");
        assert!(
            line.contains("HOST"),
            "the names still say what is set: {line}"
        );
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
        let mut vars = vars();
        vars.remove(TOKEN_VAR);
        let err = Config::parse(MINIMAL, &vars).expect_err("no token, no start");
        assert!(err.to_string().contains(TOKEN_VAR), "{err}");
    }

    #[test]
    fn an_empty_variable_is_not_a_value() {
        assert!(Config::parse(MINIMAL, &with(TOKEN_VAR, "")).is_err());
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
        let text = MINIMAL.replace("[buffer]", "[buffer]\nbatches_max = 0");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn a_site_past_the_latitude_bound_is_refused() {
        let err = Config::parse(MINIMAL, &with(LATITUDE_VAR, "70.0")).expect_err("past the bound");
        assert!(err.to_string().contains("rises and sets"), "{err}");
    }

    #[test]
    fn a_zone_the_contract_would_reject_is_refused_here() {
        let err =
            Config::parse(MINIMAL, &with(TZ_VAR, "-03:00")).expect_err("an offset is not a zone");
        assert!(err.to_string().contains(TZ_VAR), "{err}");
    }

    #[test]
    fn a_margin_past_the_ceiling_is_refused() {
        let text = MINIMAL.replace("after_sunset_min = 30 }", "after_sunset_min = 240 }");
        let err = parse(&text).expect_err("past the ceiling");
        assert!(err.to_string().contains("ceiling"), "{err}");
    }

    /// `MINIMAL` with its one source always open.
    fn windowless() -> String {
        MINIMAL.replace(
            "window = { kind = \"sun\", before_sunrise_min = 30, after_sunset_min = 30 }\n",
            "",
        )
    }

    #[test]
    fn a_source_without_a_window_is_always_open_and_needs_no_site() {
        // The site is a home address by another name: nothing that does not need it asks for it.
        let mut vars = vars();
        vars.remove(LATITUDE_VAR);
        vars.remove(LONGITUDE_VAR);
        let config = Config::parse(&windowless(), &vars).expect("parses");
        assert!(config.sources[0].window.is_none());
        let err = Config::parse(MINIMAL, &vars).expect_err("a sun window needs the site");
        assert!(format!("{err:#}").contains(LATITUDE_VAR), "{err:#}");
    }

    #[test]
    fn a_window_it_cannot_read_is_refused() {
        for window in [
            r#"window = { kind = "moon" }"#,
            r#"window = { kind = "sun", before_sunrise_min = 30 }"#,
            concat!(
                r#"window = { kind = "sun", before_sunrise_min = 30, after_sunset_min = 30, "#,
                "dusk = 1 }",
            ),
        ] {
            let text = MINIMAL.replace(
                "window = { kind = \"sun\", before_sunrise_min = 30, after_sunset_min = 30 }",
                window,
            );
            let err = parse(&text).expect_err(window);
            assert!(
                format!("{err:#}").contains("inverter"),
                "names the source: {err:#}"
            );
        }
    }

    #[test]
    fn a_device_wide_window_is_refused() {
        // It moved into the source that goes dark; a file still carrying it predates that.
        let text = format!(
            "{}\n[window]\nbefore_sunrise_min = 30\nafter_sunset_min = 30\n",
            windowless()
        );
        assert!(parse(&text).is_err());
    }

    #[test]
    fn a_source_reads_its_own_variables_by_key() {
        // The driver knows `SERIAL`; which variable that is stays here. Empty is unset.
        let vars = with("MAGELLAN_SOURCE_INVERTER_HOST", "");
        let config = Config::parse(MINIMAL, &vars).expect("parses");
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
        // Cut before the one `[[source]]`.
        let text = MINIMAL.split("[[source]]").next().unwrap_or_default();
        assert!(parse(text).is_err());
    }
}
