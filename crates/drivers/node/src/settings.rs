//! A `[[source]]` block as this driver reads it, and the node it makes.
//!
//! The runtime holds the block without reading it, so every key here is refused or defaulted here.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;

use crate::source::{Locate, Node, Timing};

/// Defaults for what a block may leave out, in the unit its key names.
const PORT: u16 = 502;
/// Not significant to a server reached directly over TCP; 0xFF is the value the TCP guide gives,
/// so a gateway that later takes this address drops the read instead of routing it.
const UNIT: u8 = 0xFF;
const CONNECT_TIMEOUT_S: u64 = 5;
const READ_TIMEOUT_S: u64 = 5;
const DISCOVERY_TIMEOUT_S: u64 = 3;

/// The block as written, `id` and `driver` already taken by the runtime.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    /// The id the node advertises over mDNS, set in its portal.
    node_id: String,
    /// Dialled with `HOST`; a found node brings its own.
    #[serde(default = "port")]
    port: u16,
    #[serde(default = "unit")]
    unit: u8,
    #[serde(rename = "connect_timeout_s", default = "connect_timeout_s")]
    connect: u64,
    #[serde(rename = "read_timeout_s", default = "read_timeout_s")]
    read: u64,
    #[serde(rename = "discovery_timeout_s", default = "discovery_timeout_s")]
    discovery: u64,
}

const fn port() -> u16 {
    PORT
}

const fn unit() -> u8 {
    UNIT
}

const fn connect_timeout_s() -> u64 {
    CONNECT_TIMEOUT_S
}

const fn read_timeout_s() -> u64 {
    READ_TIMEOUT_S
}

const fn discovery_timeout_s() -> u64 {
    DISCOVERY_TIMEOUT_S
}

/// Why a `[[source]]` block does not make a node. A startup failure: no retry fixes one.
#[derive(Debug)]
pub enum SettingsError {
    /// A key this driver does not read, a value of the wrong type, or no `node_id`.
    Invalid(toml::de::Error),
    /// An empty `node_id`: nothing advertises it.
    NoNodeId,
    /// A timeout of zero: every dial or read would time out before it began.
    Zero(&'static str),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(e) => write!(f, "{e}"),
            Self::NoNodeId => write!(f, "node_id is empty"),
            Self::Zero(key) => write!(f, "{key} is 0"),
        }
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invalid(e) => Some(e),
            Self::NoNodeId | Self::Zero(_) => None,
        }
    }
}

/// The node a `[[source]]` block describes. Touches nothing: the node is found when a poll needs
/// it.
///
/// `var` reads this source's own environment by key. With a `HOST` the node is dialled there, as
/// an address names one installation and never enters the file; without one, the node is found by
/// the id it advertises.
///
/// # Errors
///
/// [`SettingsError`] for a block this driver cannot read.
pub fn from_settings(
    id: &str,
    settings: &toml::Table,
    var: impl Fn(&str) -> Option<String>,
) -> Result<Node, SettingsError> {
    let settings: Settings = toml::Value::Table(settings.clone())
        .try_into()
        .map_err(SettingsError::Invalid)?;
    if settings.node_id.is_empty() {
        return Err(SettingsError::NoNodeId);
    }
    let timing = Timing {
        connect: timeout("connect_timeout_s", settings.connect)?,
        read: timeout("read_timeout_s", settings.read)?,
        discovery: timeout("discovery_timeout_s", settings.discovery)?,
    };
    let locate = match var("HOST") {
        Some(host) => Locate::Host(format!("{host}:{}", settings.port)),
        None => Locate::Discover {
            node_id: settings.node_id,
            found: None,
        },
    };
    Ok(Node::new(id.to_owned(), locate, settings.unit, timing))
}

fn timeout(key: &'static str, seconds: u64) -> Result<Duration, SettingsError> {
    if seconds == 0 {
        return Err(SettingsError::Zero(key));
    }
    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(text: &str, host: Option<&str>) -> Result<Node, SettingsError> {
        let block = toml::from_str(text).expect("a toml table");
        from_settings("garage", &block, |key| {
            (key == "HOST").then_some(host).flatten().map(str::to_owned)
        })
    }

    #[test]
    fn a_block_naming_only_its_node_takes_every_default() {
        let node = open(r#"node_id = "garage""#, None).expect("reads");
        assert_eq!(
            node.timing(),
            Timing {
                connect: Duration::from_secs(5),
                read: Duration::from_secs(5),
                discovery: Duration::from_secs(3),
            }
        );
        assert_eq!(
            node.locate(),
            &Locate::Discover {
                node_id: "garage".to_owned(),
                found: None
            }
        );
    }

    #[test]
    fn a_host_is_dialled_on_the_port_the_block_names() {
        let node = open("node_id = \"garage\"\nport = 1502", Some("10.0.0.7")).expect("reads");
        assert_eq!(node.locate(), &Locate::Host("10.0.0.7:1502".to_owned()));
    }

    #[test]
    fn a_key_this_driver_does_not_read_is_refused() {
        let refused = open("node_id = \"garage\"\nprofile = \"sofar-g3\"", None);
        assert!(matches!(refused, Err(SettingsError::Invalid(_))));
    }

    #[test]
    fn a_block_without_a_node_is_refused() {
        assert!(matches!(open("", None), Err(SettingsError::Invalid(_))));
        assert!(matches!(
            open(r#"node_id = """#, None),
            Err(SettingsError::NoNodeId)
        ));
    }

    #[test]
    fn the_example_block_reads_and_shows_the_defaults() {
        // Its comments say a key left out takes the value shown; this holds it to that.
        let example: toml::Table =
            toml::from_str(include_str!("../../../../config.example.toml")).expect("parses");
        let mut shown = example
            .get("source")
            .and_then(toml::Value::as_array)
            .expect("a [[source]]")
            .iter()
            .filter_map(toml::Value::as_table)
            .find(|block| block.get("driver").and_then(toml::Value::as_str) == Some("node"))
            .expect("a node block")
            .clone();
        // The runtime's keys, taken out before a driver sees the block.
        shown.remove("id");
        shown.remove("driver");
        let shown = from_settings("garage", &shown, |_| None).expect("reads");
        let defaults = open(r#"node_id = "garage""#, None).expect("reads");
        assert_eq!(shown.timing(), defaults.timing());
        assert_eq!(shown.locate(), defaults.locate());
    }

    #[test]
    fn a_zero_timeout_is_refused() {
        let refused = open("node_id = \"garage\"\nread_timeout_s = 0", None);
        assert!(matches!(
            refused,
            Err(SettingsError::Zero("read_timeout_s"))
        ));
    }
}
