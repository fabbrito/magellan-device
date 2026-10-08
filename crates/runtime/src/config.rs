//! What the device is told at startup, and what it refuses to start without.
//!
//! The file is the installation: the device's id, zone and site, every source and what finds it.
//! It is rendered per device and never committed — `config.example.toml` is the shape, with
//! placeholders. The environment carries only the device token: a credential, kept where the
//! service manager reads it before the process runs.
//!
//! Unknown keys are rejected, so a typo fails at startup rather than silently taking a default. A
//! timing left out takes its default, so a file written before it was a setting still reads.

use std::collections::BTreeSet;
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
use crate::schedule::sun::{LATITUDE_DEG_MAX, Site};
use crate::schedule::window::Sun;

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
/// The one variable the device reads: its credential.
const TOKEN_VAR: &str = "MAGELLAN_TOKEN";

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
}

/// Names its settings, never their values: a serial or an address names one installation, and a
/// journal is copied, shipped and pasted into issues.
impl fmt::Debug for SourceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceConfig")
            .field("id", &self.id)
            .field("driver", &self.driver)
            .field("settings", &self.settings.keys().collect::<Vec<_>>())
            .field("window", &self.window.is_some())
            .finish()
    }
}

/// The file as written.
#[cfg_attr(test, derive(Debug, PartialEq))]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    device: RawDevice,
    cloud: RawCloud,
    poll: RawPoll,
    #[serde(default)]
    drain: RawDrain,
    buffer: RawBuffer,
    #[serde(default)]
    heartbeat: RawHeartbeat,
    source: Vec<RawSource>,
}

/// The installation itself. The site is read only when a window needs it: a device with no sun
/// window names no location.
#[cfg_attr(test, derive(Debug, PartialEq))]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDevice {
    id: String,
    tz: String,
    latitude: Option<f64>,
    longitude: Option<f64>,
}

#[cfg_attr(test, derive(Debug, PartialEq))]
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

#[cfg_attr(test, derive(Debug, PartialEq))]
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

#[cfg_attr(test, derive(Debug, PartialEq))]
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

#[cfg_attr(test, derive(Debug, PartialEq))]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPoll {
    sweep_period_s: u64,
}

#[cfg_attr(test, derive(Debug, PartialEq))]
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

#[cfg_attr(test, derive(Debug, PartialEq))]
#[derive(Deserialize)]
struct RawSource {
    id: String,
    driver: String,
    #[serde(flatten)]
    settings: toml::Table,
}

impl Config {
    /// Read the file at `path`, and the token from the environment.
    ///
    /// # Errors
    ///
    /// If the file cannot be read or parsed, if it carries a key the schema does not know, if a
    /// bound is broken, or if the token is missing.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        // Read once, here. Writing the environment is unsound once a thread runs, so nothing
        // after startup reads it either, and a test hands `parse` the value instead.
        let token = env::var_os(TOKEN_VAR).and_then(|token| token.into_string().ok());
        Self::parse(&text, token.as_deref())
    }

    /// The same, from text and a token already in hand.
    ///
    /// # Errors
    ///
    /// As [`Config::load`], less the reading.
    pub(crate) fn parse(text: &str, token: Option<&str>) -> Result<Self> {
        let raw: Raw = toml::from_str(text).context("parsing the configuration")?;
        Self::from_raw(raw, token)
    }

    /// What the file says, checked: every bound, every timing, the token. Apart from the parse so
    /// a rule is tested on the value it judges, not on text that has to spell it first.
    fn from_raw(raw: Raw, token: Option<&str>) -> Result<Self> {
        check_endpoint(&raw.cloud.endpoint)?;
        let cadence = read_cadence(&raw)?;
        let request_timeout = seconds("cloud.request_timeout_s", raw.cloud.request_timeout_s)?;
        let buffer = NonZeroUsize::new(raw.buffer.batches_max)
            .context("buffer.batches_max is 0, so every reading is dropped as it is made")?;
        let token = read_token(token)?;
        let device = raw.device;
        ensure!(!device.id.is_empty(), "device.id is empty");
        ensure!(
            zone_is_known(&device.tz),
            "device.tz is {:?}, not an IANA zone spelled as the tz database spells it",
            device.tz
        );
        let sources = read_sources(raw.source, &device)?;
        Ok(Self {
            device_id: device.id,
            token,
            endpoint: raw.cloud.endpoint,
            request_timeout,
            cadence,
            buffer,
            buffer_dir: raw.buffer.dir,
            zone: device.tz,
            sources,
        })
    }
}

/// Every source as written, checked against the contract's bounds and each other, its window read.
fn read_sources(raw: Vec<RawSource>, device: &RawDevice) -> Result<Vec<SourceConfig>> {
    ensure!(!raw.is_empty(), "no [[source]] to read");
    ensure!(
        raw.len() <= SOURCES_MAX,
        "{} sources, past the contract's {SOURCES_MAX}",
        raw.len()
    );
    let mut seen = BTreeSet::new();
    let mut sources = Vec::with_capacity(raw.len());
    // Read once, and only when a window needs it: a device with no sun window names no site.
    let mut site: Option<Site> = None;
    for mut source in raw {
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
                    None => *site.insert(read_site(device)?),
                };
                Some(read_window(&source.id, written, site)?)
            }
        };
        sources.push(SourceConfig {
            id: source.id,
            driver: source.driver,
            settings: source.settings,
            window,
        });
    }
    Ok(sources)
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

/// The site's coordinates: where the sun rises is where the installation is, and that is a home
/// address by another name.
fn read_site(device: &RawDevice) -> Result<Site> {
    let (Some(latitude), Some(longitude)) = (device.latitude, device.longitude) else {
        bail!("a sun window needs the site: device.latitude and device.longitude");
    };
    ensure!(
        latitude.abs() <= LATITUDE_DEG_MAX,
        "device.latitude is {latitude}, past the {LATITUDE_DEG_MAX} degrees where the sun still \
         rises and sets every day"
    );
    ensure!(
        longitude.abs() <= 180.0,
        "device.longitude is {longitude}, which is not a longitude"
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

fn read_token(token: Option<&str>) -> Result<Token> {
    match token {
        Some(token) if !token.is_empty() => Ok(Token(token.to_owned())),
        Some(_) => bail!("{TOKEN_VAR} is set but empty"),
        None => bail!("{TOKEN_VAR} is not set; the credential is never in the file"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [device]
        id = "device_1"
        tz = "America/Sao_Paulo"
        latitude = -23.55
        longitude = -46.63

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

    const TOKEN: &str = "s3cret";

    fn parse(text: &str) -> Result<Config> {
        Config::parse(text, Some(TOKEN))
    }

    /// What `MINIMAL` says, built in Rust: the base every rule is tested against.
    fn minimal() -> Raw {
        Raw {
            device: RawDevice {
                id: "device_1".to_owned(),
                tz: "America/Sao_Paulo".to_owned(),
                latitude: Some(-23.55),
                longitude: Some(-46.63),
            },
            cloud: RawCloud {
                endpoint: "https://cloud.example/v1".to_owned(),
                request_timeout_s: REQUEST_TIMEOUT_S,
            },
            poll: RawPoll {
                sweep_period_s: 300,
            },
            drain: RawDrain::default(),
            buffer: RawBuffer {
                dir: PathBuf::from("/var/lib/magellan"),
                batches_max: BATCHES_MAX,
            },
            heartbeat: RawHeartbeat::default(),
            source: vec![inverter()],
        }
    }

    /// `MINIMAL`'s one source: a sofar inverter behind a sun window.
    fn inverter() -> RawSource {
        RawSource {
            id: "inverter".to_owned(),
            driver: "sofar".to_owned(),
            settings: toml::Table::from_iter([
                ("window".to_owned(), sun(30, 30)),
                ("profile".to_owned(), "sofar-g3".into()),
                ("port".to_owned(), 8899.into()),
            ]),
        }
    }

    /// A sun window with these margins, as a `[[source]]` holds it.
    fn sun(before_sunrise_min: i64, after_sunset_min: i64) -> toml::Value {
        toml::Value::Table(toml::Table::from_iter([
            ("kind".to_owned(), "sun".into()),
            ("before_sunrise_min".to_owned(), before_sunrise_min.into()),
            ("after_sunset_min".to_owned(), after_sunset_min.into()),
        ]))
    }

    /// One change to `minimal()`, as a table of cases spells it.
    type Change = fn(&mut Raw);

    /// `minimal()`, changed as `change` says, checked.
    fn checked(change: impl FnOnce(&mut Raw)) -> Result<Config> {
        let mut raw = minimal();
        change(&mut raw);
        Config::from_raw(raw, Some(TOKEN))
    }

    /// The first source's settings: where its window is.
    fn first_settings(raw: &mut Raw) -> &mut toml::Table {
        &mut raw
            .source
            .first_mut()
            .expect("minimal has a source")
            .settings
    }

    #[test]
    fn the_text_base_and_the_typed_base_are_one() {
        // The rules are tested on `minimal()`, the parsing on `MINIMAL`: drift apart and neither
        // proves the other.
        let parsed: Raw = toml::from_str(MINIMAL).expect("parses");
        assert_eq!(parsed, minimal());
    }

    #[test]
    fn a_minimal_config_reads() {
        let config = checked(|_| ()).expect("reads");
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
        let cases: [(&str, Change); 6] = [
            ("pace_s", |raw| raw.drain.pace = 0),
            ("backoff_first_s", |raw| raw.drain.backoff_first = 0),
            ("period_s", |raw| raw.heartbeat.period = 0),
            ("sweep_period_s", |raw| raw.poll.sweep_period_s = 0),
            ("request_timeout_s", |raw| raw.cloud.request_timeout_s = 0),
            ("recheck_min", |raw| {
                let window = first_settings(raw)
                    .get_mut("window")
                    .and_then(toml::Value::as_table_mut);
                window
                    .expect("a window")
                    .insert("recheck_min".to_owned(), 0.into());
            }),
        ];
        for (key, zero) in cases {
            let refused = checked(zero).expect_err(key).to_string();
            assert!(refused.contains(key), "{refused}");
        }
    }

    #[test]
    fn a_backoff_that_starts_above_its_ceiling_is_refused() {
        assert!(checked(|raw| raw.drain.backoff_first = 600).is_err());
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
        let config = checked(|_| ()).expect("reads");
        assert_eq!(format!("{:?}", config.token), "Token(redacted)");
        assert!(!format!("{config:?}").contains("s3cret"));
        assert_eq!(config.token.reveal(), "s3cret");
    }

    #[test]
    fn a_sources_settings_never_show_up_in_a_debug_line() {
        // A serial and a home address name one installation, as the token names one device.
        let config = checked(|raw| {
            let settings = first_settings(raw);
            settings.insert("serial".to_owned(), 3_735_928_559_i64.into());
            settings.insert("host".to_owned(), "192.0.2.7".into());
        });
        let line = format!("{:?}", config.expect("reads"));
        assert!(!line.contains("3735928559"), "{line}");
        assert!(!line.contains("192.0.2.7"), "{line}");
        assert!(
            line.contains("host"),
            "the keys still say what is set: {line}"
        );
    }

    #[test]
    fn a_cleartext_endpoint_is_refused_unless_it_is_loopback() {
        let plain = checked(|raw| raw.cloud.endpoint = "http://cloud.example/v1".to_owned());
        let err = plain.expect_err("http must be refused");
        assert!(err.to_string().contains("device token"), "{err}");

        // The fake cloud the tests reach for never leaves the machine.
        let local = checked(|raw| raw.cloud.endpoint = "http://127.0.0.1:8080".to_owned());
        assert!(local.is_ok());
    }

    #[test]
    fn a_missing_token_stops_the_device_at_startup() {
        let err = Config::from_raw(minimal(), None).expect_err("no token, no start");
        assert!(err.to_string().contains(TOKEN_VAR), "{err}");
    }

    #[test]
    fn an_empty_token_is_not_a_value() {
        assert!(Config::from_raw(minimal(), Some("")).is_err());
    }

    #[test]
    fn a_device_with_an_empty_id_is_refused() {
        assert!(checked(|raw| raw.device.id.clear()).is_err());
    }

    #[test]
    fn a_device_without_an_id_does_not_parse() {
        assert!(parse(&MINIMAL.replace("id = \"device_1\"\n", "")).is_err());
    }

    #[test]
    fn two_sources_cannot_share_an_id() {
        let err = checked(|raw| raw.source.push(inverter())).expect_err("duplicate ids");
        assert!(err.to_string().contains("share the id"), "{err}");
    }

    #[test]
    fn two_nodes_are_two_sources_each_with_its_own_block() {
        // An edge reads many nodes; nothing one block says may reach another.
        let text = format!(
            "{MINIMAL}
            [[source]]
            id = \"garage\"
            driver = \"node\"
            node_id = \"garage\"

            [[source]]
            id = \"attic\"
            driver = \"node\"
            node_id = \"attic\"
            host = \"192.0.2.12\"
            "
        );
        let config = parse(&text).expect("parses");
        let nodes: Vec<_> = config
            .sources
            .iter()
            .filter(|source| source.driver == "node")
            .map(|source| {
                let key = |key: &str| source.settings.get(key).and_then(toml::Value::as_str);
                (source.id.as_str(), key("node_id"), key("host"))
            })
            .collect();
        assert_eq!(
            nodes,
            [
                ("garage", Some("garage"), None),
                ("attic", Some("attic"), Some("192.0.2.12")),
            ]
        );
    }

    #[test]
    fn a_source_id_the_contract_would_reject_is_refused_here() {
        // Better at startup than at the first upload, where it costs a round trip and a 4xx.
        let bad = checked(|raw| {
            raw.source.first_mut().expect("minimal has a source").id = "inverter/1".to_owned();
        });
        let err = bad.expect_err("a bad id must be refused");
        assert!(err.to_string().contains("contract accepts"), "{err}");
    }

    #[test]
    fn a_buffer_that_holds_nothing_is_refused() {
        assert!(checked(|raw| raw.buffer.batches_max = 0).is_err());
    }

    #[test]
    fn a_site_past_the_latitude_bound_is_refused() {
        let err = checked(|raw| raw.device.latitude = Some(70.0)).expect_err("past the bound");
        assert!(err.to_string().contains("rises and sets"), "{err}");
    }

    #[test]
    fn a_zone_the_contract_would_reject_is_refused_here() {
        let err = checked(|raw| raw.device.tz = "-03:00".to_owned()).expect_err("not a zone");
        assert!(err.to_string().contains("device.tz"), "{err}");
    }

    #[test]
    fn a_margin_past_the_ceiling_is_refused() {
        let err = checked(|raw| {
            first_settings(raw).insert("window".to_owned(), sun(30, 240));
        })
        .expect_err("past the ceiling");
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
        let siteless = |raw: &mut Raw| {
            raw.device.latitude = None;
            raw.device.longitude = None;
        };
        let config = checked(|raw| {
            siteless(raw);
            first_settings(raw).remove("window");
        })
        .expect("reads");
        assert!(config.sources[0].window.is_none());
        let err = checked(siteless).expect_err("a sun window needs the site");
        assert!(format!("{err:#}").contains("device.latitude"), "{err:#}");
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
    fn a_config_with_no_source_reads_nothing_and_is_refused() {
        assert!(checked(|raw| raw.source.clear()).is_err());
    }
}
