//! A Sofar inverter, read over Modbus TCP through its LSW-3 logger.
//!
//! One crate holds the whole driver: the wire below, the register map above. They are separate
//! modules and not separate crates, because a second device behind the same logger is what would
//! tell us where the seam between them goes — and there is not one yet.
//!
//! The logger also emits frames in its own protocol on this socket, unasked and unrelated to any
//! read. The decoder recognises and steps over them; nothing here speaks it.

#[cfg(test)]
mod captures;
mod common;
mod decode;
mod discover;
mod error;
mod frame;
mod modbus;
mod profile;
mod session;
mod settings;
mod source;

pub use crate::settings::{SettingsError, from_settings};

use crate::error::ProfileError;
use crate::profile::Profile;

/// Profiles shipped in the binary, by name. Another inverter family is another file.
const BUILTIN: &[(&str, &str)] = &[("sofar-g3", include_str!("../profiles/sofar-g3.toml"))];

/// Load a shipped profile.
///
/// # Errors
///
/// [`ProfileError::Unknown`] if no profile has that name; otherwise whatever [`Profile::parse`]
/// rejects.
pub(crate) fn builtin(name: &str) -> Result<Profile, ProfileError> {
    let (_, text) = BUILTIN
        .iter()
        .find(|(n, _)| *n == name)
        .ok_or_else(|| ProfileError::Unknown(name.to_owned()))?;
    Profile::parse(text)
}
