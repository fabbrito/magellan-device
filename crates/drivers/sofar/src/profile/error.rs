use std::fmt;

/// Why a profile could not be used.
///
/// Separate from [`WireError`](crate::wire::WireError) on purpose: a bad profile is a startup
/// failure that no amount of retrying fixes, while every `WireError` is a fact about one read.
#[derive(Debug)]
pub enum ProfileError {
    /// No shipped profile has this name.
    Unknown(String),
    /// Not valid TOML, or a key the schema does not know.
    Parse(toml::de::Error),
    /// Parsed, but breaks a rule decode depends on.
    Invalid(String),
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(name) => write!(f, "no inverter profile named {name:?}"),
            Self::Parse(e) => write!(f, "parsing profile: {e}"),
            Self::Invalid(why) => write!(f, "invalid profile: {why}"),
        }
    }
}

impl std::error::Error for ProfileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(e) => Some(e),
            _ => None,
        }
    }
}
