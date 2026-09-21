//! What the device is told at startup, and what it refuses to start without.
//!
//! Two sources, deliberately apart. The file holds settings that describe the *deployment* and
//! are safe to commit; the environment holds everything that identifies one *installation* — the
//! device token, the site's coordinates, a logger's serial and address. `AGENTS.md` keeps
//! credentials and home-network details out of git, and a schema that cannot express them is a
//! stronger guarantee than remembering not to write them down.
//!
//! Unknown keys are rejected, so a typo fails at startup rather than silently taking a default.

use std::collections::BTreeSet;
use std::env;
use std::fmt;
use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use contract::key_is_well_formed;
use contract::limits::SOURCES_MAX;
use jiff::SignedDuration;
use serde::Deserialize;

use crate::sun::{MAX_LATITUDE_DEG, Site};

/// Past three hours, a margin polls a dark source for most of the night.
const MARGIN_MAX_MIN: u32 = 180;
/// Environment variables carrying the per-installation identity.
const DEVICE_ID_VAR: &str = "MAGELLAN_DEVICE_ID";
const TOKEN_VAR: &str = "MAGELLAN_TOKEN";
const LATITUDE_VAR: &str = "MAGELLAN_LATITUDE";
const LONGITUDE_VAR: &str = "MAGELLAN_LONGITUDE";

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
    pub sweep_period: Duration,
    pub buffer: NonZeroUsize,
    pub margins: Margins,
    pub site: Site,
    pub sources: Vec<SourceConfig>,
}

/// How far either side of daylight the device keeps polling.
#[derive(Debug, Clone, Copy)]
pub struct Margins {
    pub before_sunrise: SignedDuration,
    pub after_sunset: SignedDuration,
}

/// One source to construct at boot.
#[derive(Debug)]
pub struct SourceConfig {
    /// Becomes `Source::id` in the manifest, so the contract's pattern binds it.
    pub id: String,
    /// Which driver reads it.
    pub driver: String,
    /// The driver's own settings, as written.
    pub settings: toml::Table,
    /// From the environment: `MAGELLAN_SOURCE_<ID>_SERIAL`.
    pub serial: u32,
    /// From the environment: `MAGELLAN_SOURCE_<ID>_HOST`, when discovery is not to be used.
    pub host: Option<String>,
}

/// The file as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    cloud: RawCloud,
    poll: RawPoll,
    buffer: RawBuffer,
    window: RawWindow,
    source: Vec<RawSource>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCloud {
    endpoint: String,
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
    before_sunrise_min: u32,
    after_sunset_min: u32,
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
        ensure!(
            raw.poll.sweep_period_s > 0,
            "poll.sweep_period_s is 0, which is no schedule at all"
        );
        for (name, minutes) in [
            ("before_sunrise_min", raw.window.before_sunrise_min),
            ("after_sunset_min", raw.window.after_sunset_min),
        ] {
            ensure!(
                minutes <= MARGIN_MAX_MIN,
                "window.{name} is {minutes}, past the {MARGIN_MAX_MIN} minute ceiling"
            );
        }
        let buffer = NonZeroUsize::new(raw.buffer.batches_max)
            .context("buffer.batches_max is 0, so every reading is dropped as it is made")?;
        let site = read_site()?;

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
            sources.push(read_source(source)?);
        }

        Ok(Self {
            device_id: read_var(DEVICE_ID_VAR)?,
            token: Token(read_var(TOKEN_VAR)?),
            endpoint: raw.cloud.endpoint,
            sweep_period: Duration::from_secs(raw.poll.sweep_period_s),
            buffer,
            margins: Margins {
                before_sunrise: SignedDuration::from_mins(i64::from(raw.window.before_sunrise_min)),
                after_sunset: SignedDuration::from_mins(i64::from(raw.window.after_sunset_min)),
            },
            site,
            sources,
        })
    }
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
        latitude.abs() <= MAX_LATITUDE_DEG,
        "{LATITUDE_VAR} is {latitude}, past the {MAX_LATITUDE_DEG} degrees where the sun still \
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

/// A source's identity, from the environment: a serial names one unit and an address is a
/// home-network detail, so neither belongs in a file meant to be committed.
fn read_source(raw: RawSource) -> Result<SourceConfig> {
    let upper = raw.id.to_uppercase().replace(['-', '.', ':'], "_");
    let serial_var = format!("MAGELLAN_SOURCE_{upper}_SERIAL");
    let host_var = format!("MAGELLAN_SOURCE_{upper}_HOST");
    let serial = read_var(&serial_var)?
        .parse::<u32>()
        .with_context(|| format!("{serial_var} is not a serial number"))?;
    Ok(SourceConfig {
        id: raw.id,
        driver: raw.driver,
        settings: raw.settings,
        serial,
        host: env::var(&host_var).ok().filter(|h| !h.is_empty()),
    })
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
        assert_eq!(config.sweep_period, Duration::from_secs(300));
        assert_eq!(config.buffer.get(), 64);
        assert_eq!(config.sources.len(), 1);
        let source = &config.sources[0];
        assert_eq!(
            (source.id.as_str(), source.driver.as_str()),
            ("inverter", "sofar")
        );
        assert_eq!(source.serial, 3_735_928_559);
        // What the schema does not name stays as the driver's to read.
        assert_eq!(source.settings["port"].as_integer(), Some(8899));
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
    fn a_margin_past_the_ceiling_is_refused() {
        let text = MINIMAL.replace("after_sunset_min = 30", "after_sunset_min = 240");
        let err = parse(&text).expect_err("past the ceiling");
        assert!(err.to_string().contains("ceiling"), "{err}");
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
