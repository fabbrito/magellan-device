//! A Sofar inverter, read over Modbus TCP through its LSW-3 logger.
//!
//! One crate holds the whole driver: the wire below, the register map above. They are separate
//! modules and not separate crates, because a second device behind the same logger is what would
//! tell us where the seam between them goes — and there is not one yet.
//!
//! The logger also emits frames in its own protocol on this socket, unasked and unrelated to any
//! read. The decoder recognises and steps over them; nothing here speaks it.

mod common;
mod decode;
pub mod discover;
mod error;
mod frame;
mod modbus;
mod profile;
mod session;
mod source;

pub use crate::common::canonical_unit;
pub use crate::decode::{Decoded, NamedValue, NamedValues, Value};
pub use crate::error::{Error, ProfileError};
pub use crate::frame::{Frame, FrameCodec, ReadRequest, next_frame_tcp};
pub use crate::modbus::registers;
pub use crate::profile::{Entry, Kind, Profile, Range};
pub use crate::session::{Exchange, Outcome, Session};
pub use crate::source::{Inverter, READ_GAP_MIN, Timing};

/// Profiles shipped in the binary, by name. Another inverter family is another file.
const BUILTIN: &[(&str, &str)] = &[("sofar-g3", include_str!("../profiles/sofar-g3.toml"))];

/// Load a shipped profile.
///
/// # Errors
///
/// [`ProfileError::Unknown`] if no profile has that name; otherwise whatever [`Profile::parse`]
/// rejects.
pub fn builtin(name: &str) -> Result<Profile, ProfileError> {
    let (_, text) = BUILTIN
        .iter()
        .find(|(n, _)| *n == name)
        .ok_or_else(|| ProfileError::Unknown(name.to_owned()))?;
    Profile::parse(text)
}
