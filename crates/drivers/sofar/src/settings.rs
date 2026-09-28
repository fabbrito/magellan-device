//! A `[[source]]` block as this driver reads it, and the inverter it makes.
//!
//! The runtime holds the block without reading it, so every key here is refused or defaulted here:
//! a misspelled `port` would otherwise take the default and read nothing, quietly, for as long as
//! nobody looked.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;

use crate::error::ProfileError;
use crate::source::{Inverter, Locate, READ_GAP_MIN, Timing};
use crate::{builtin, discover};

/// Defaults for what a block may leave out, in the unit its key names.
const PORT: u16 = 8899;
const SLAVE: u8 = 1;
const CONNECT_TIMEOUT_S: u64 = 10;
const READ_TIMEOUT_S: u64 = 20;
const READ_GAP_S: u64 = 15;
const DISCOVERY_TIMEOUT_S: u64 = 3;

/// The block as written, `id` and `driver` already taken by the runtime.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    /// Read plan and register names, shipped in the binary.
    profile: String,
    #[serde(default = "port")]
    port: u16,
    #[serde(default = "slave")]
    slave: u8,
    /// Longest a dial may take before the address counts as dark.
    #[serde(rename = "connect_timeout_s", default = "connect_timeout_s")]
    connect: u64,
    /// Longest one range read may take. Must clear the slowest refusal, not the typical one.
    #[serde(rename = "read_timeout_s", default = "read_timeout_s")]
    read: u64,
    /// Gap between reads inside a sweep; never under [`READ_GAP_MIN`].
    #[serde(rename = "read_gap_s", default = "read_gap_s")]
    gap: u64,
    /// How long a logger gets to answer the discovery hello. Only a dark one takes it all.
    #[serde(rename = "discovery_timeout_s", default = "discovery_timeout_s")]
    discovery: u64,
}

const fn port() -> u16 {
    PORT
}

const fn slave() -> u8 {
    SLAVE
}

const fn connect_timeout_s() -> u64 {
    CONNECT_TIMEOUT_S
}

const fn read_timeout_s() -> u64 {
    READ_TIMEOUT_S
}

const fn read_gap_s() -> u64 {
    READ_GAP_S
}

const fn discovery_timeout_s() -> u64 {
    DISCOVERY_TIMEOUT_S
}

/// Why a `[[source]]` block does not make an inverter. A startup failure: no retry fixes one.
#[derive(Debug)]
pub enum SettingsError {
    /// A key this driver does not read, a value of the wrong type, or no `profile`.
    Invalid(toml::de::Error),
    /// The profile named cannot be used.
    Profile(ProfileError),
    /// A timeout of zero: every dial or read would time out before it began.
    Zero(&'static str),
    /// A read gap under the floor the logger has been seen to need.
    GapBelowFloor(u64),
    /// Neither `HOST` nor `SERIAL`: the logger can be neither dialled nor found.
    NoAddress,
    /// `SERIAL` is not a serial number. The value is not kept: it names one installation.
    Serial,
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(e) => write!(f, "{e}"),
            Self::Profile(e) => write!(f, "{e}"),
            Self::Zero(key) => write!(f, "{key} is 0"),
            Self::GapBelowFloor(seconds) => write!(
                f,
                "read_gap_s is {seconds}, under the {}s the logger has been seen to need",
                READ_GAP_MIN.as_secs()
            ),
            Self::NoAddress => write!(
                f,
                "neither HOST nor SERIAL is set, so the logger can be neither dialled nor found"
            ),
            Self::Serial => write!(
                f,
                "SERIAL is not a serial number; decimal or 0x-prefixed hex"
            ),
        }
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invalid(e) => Some(e),
            Self::Profile(e) => Some(e),
            _ => None,
        }
    }
}

/// The inverter a `[[source]]` block describes. Touches nothing: the logger is found when a sweep
/// needs it.
///
/// `var` reads this source's own environment by key — `HOST`, `SERIAL` — since a serial and an
/// address name one installation and never enter the file. With a `HOST` the logger is dialled
/// there; without one, `SERIAL` is what discovery looks for.
///
/// # Errors
///
/// [`SettingsError`] for a block or an environment this driver cannot read.
pub fn from_settings(
    id: &str,
    settings: &toml::Table,
    var: impl Fn(&str) -> Option<String>,
) -> Result<Inverter, SettingsError> {
    let settings: Settings = toml::Value::Table(settings.clone())
        .try_into()
        .map_err(SettingsError::Invalid)?;
    ensure_gap(settings.gap)?;
    let timing = Timing {
        connect: timeout("connect_timeout_s", settings.connect)?,
        read: timeout("read_timeout_s", settings.read)?,
        gap: Duration::from_secs(settings.gap),
        discovery: timeout("discovery_timeout_s", settings.discovery)?,
    };
    let locate = match var("HOST") {
        Some(host) => Locate::Host(format!("{host}:{}", settings.port)),
        None => Locate::Discover {
            serial: parse_serial(&var("SERIAL").ok_or(SettingsError::NoAddress)?)?,
            port: settings.port,
            targets: discover::broadcast_targets(),
        },
    };
    let profile = builtin(&settings.profile).map_err(SettingsError::Profile)?;
    Ok(Inverter::new(
        id.to_owned(),
        profile,
        locate,
        settings.slave,
        timing,
    ))
}

fn ensure_gap(seconds: u64) -> Result<(), SettingsError> {
    if Duration::from_secs(seconds) < READ_GAP_MIN {
        return Err(SettingsError::GapBelowFloor(seconds));
    }
    Ok(())
}

fn timeout(key: &'static str, seconds: u64) -> Result<Duration, SettingsError> {
    if seconds == 0 {
        return Err(SettingsError::Zero(key));
    }
    Ok(Duration::from_secs(seconds))
}

/// A serial as the environment spells it: decimal, or hex with an `0x` prefix — the form the
/// logger's own web UI shows, so pasting from it must not need a conversion first.
fn parse_serial(raw: &str) -> Result<u32, SettingsError> {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .map_or_else(|| trimmed.parse(), |digits| u32::from_str_radix(digits, 16))
        .map_err(|_| SettingsError::Serial)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(text: &str) -> toml::Table {
        toml::from_str(text).expect("a toml table")
    }

    fn serial(key: &str) -> Option<String> {
        (key == "SERIAL").then(|| "3735928559".to_owned())
    }

    fn open(text: &str) -> Result<Inverter, SettingsError> {
        from_settings("inverter", &block(text), serial)
    }

    #[test]
    fn a_block_naming_only_its_profile_takes_every_default() {
        let inverter = open(r#"profile = "sofar-g3""#).expect("reads");
        let timing = inverter.timing();
        assert_eq!(timing.connect, Duration::from_secs(10));
        assert_eq!(timing.read, Duration::from_secs(20));
        assert_eq!(timing.gap, Duration::from_secs(15));
        assert_eq!(timing.discovery, Duration::from_secs(3));
        assert!(matches!(
            inverter.locate(),
            Locate::Discover {
                serial: 0xDEAD_BEEF,
                port: 8899,
                ..
            }
        ));
    }

    #[test]
    fn a_key_this_driver_does_not_read_is_refused() {
        // A misspelled `port` that took the default would dial the wrong port, quietly.
        let refused = open("profile = \"sofar-g3\"\nprot = 8899").expect_err("prot");
        assert!(matches!(refused, SettingsError::Invalid(_)));
        assert!(refused.to_string().contains("prot"), "{refused}");
    }

    #[test]
    fn a_block_without_a_profile_is_refused() {
        assert!(matches!(open(""), Err(SettingsError::Invalid(_))));
    }

    #[test]
    fn a_profile_not_shipped_is_refused() {
        assert!(matches!(
            open(r#"profile = "sofar-g9""#),
            Err(SettingsError::Profile(_))
        ));
    }

    #[test]
    fn a_read_gap_under_the_floor_is_refused() {
        // Below it the logger wedged into refusing every other read.
        assert!(matches!(
            open("profile = \"sofar-g3\"\nread_gap_s = 9"),
            Err(SettingsError::GapBelowFloor(9))
        ));
        assert!(open("profile = \"sofar-g3\"\nread_gap_s = 10").is_ok());
    }

    #[test]
    fn a_zero_timeout_is_refused() {
        for key in ["connect_timeout_s", "read_timeout_s", "discovery_timeout_s"] {
            let refused = open(&format!("profile = \"sofar-g3\"\n{key} = 0")).expect_err(key);
            assert!(
                matches!(refused, SettingsError::Zero(k) if k == key),
                "{refused}"
            );
        }
    }

    #[test]
    fn a_slave_id_past_a_modbus_unit_is_refused() {
        assert!(matches!(
            open("profile = \"sofar-g3\"\nslave = 256"),
            Err(SettingsError::Invalid(_))
        ));
    }

    #[test]
    fn a_host_is_dialled_and_needs_no_serial() {
        // The serial is only what discovery looks for.
        let inverter = from_settings(
            "inverter",
            &block("profile = \"sofar-g3\"\nport = 502"),
            |key| (key == "HOST").then(|| "192.0.2.10".to_owned()),
        )
        .expect("reads");
        assert!(matches!(inverter.locate(), Locate::Host(addr) if addr == "192.0.2.10:502"));
    }

    #[test]
    fn neither_host_nor_serial_is_refused() {
        assert!(matches!(
            from_settings("inverter", &block(r#"profile = "sofar-g3""#), |_| None),
            Err(SettingsError::NoAddress)
        ));
    }

    #[test]
    fn a_hex_serial_reads_the_same_as_its_decimal() {
        // The logger's web UI shows hex; the reply to discovery carries decimal.
        assert_eq!(parse_serial("3735928559").ok(), Some(0xDEAD_BEEF));
        assert_eq!(parse_serial("0xDEADBEEF").ok(), Some(0xDEAD_BEEF));
        assert_eq!(parse_serial(" 0Xdeadbeef ").ok(), Some(0xDEAD_BEEF));
        assert!(matches!(parse_serial("0xnope"), Err(SettingsError::Serial)));
        assert!(matches!(
            parse_serial("4294967296"),
            Err(SettingsError::Serial)
        ));
    }

    #[test]
    fn the_example_block_reads_and_shows_the_defaults() {
        // Its comments say a key left out takes the value shown; this holds it to that.
        let example: toml::Table =
            toml::from_str(include_str!("../../../../config.example.toml")).expect("parses");
        let mut blocks = example
            .get("source")
            .and_then(toml::Value::as_array)
            .expect("a [[source]]")
            .iter()
            .filter_map(toml::Value::as_table)
            .cloned();
        let mut shown = blocks.next().expect("one block");
        shown.remove("id");
        shown.remove("driver");
        let shown = from_settings("inverter", &shown, serial).expect("reads");
        let defaults = open(r#"profile = "sofar-g3""#).expect("reads");
        assert_eq!(
            format!("{:?}", shown.timing()),
            format!("{:?}", defaults.timing())
        );
        assert_eq!(
            format!("{:?}", shown.locate()),
            format!("{:?}", defaults.locate())
        );
    }
}
