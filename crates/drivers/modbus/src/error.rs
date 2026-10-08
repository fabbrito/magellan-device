//! What a read reply can be instead of registers.

use std::fmt;

#[derive(Debug)]
pub enum ModbusError {
    /// The server declined: an exception reply (unit, function | 0x80, code).
    Exception { function: u8, code: ExceptionCode },
    /// Not the reply to the read asked: a body that is not well-formed, a function the read did not
    /// send, or a byte count other than two per register asked.
    Malformed,
}

impl fmt::Display for ModbusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exception { function, code } => {
                write!(f, "modbus exception {code} on function {function}")
            }
            Self::Malformed => write!(f, "malformed read reply body"),
        }
    }
}

impl std::error::Error for ModbusError {}

/// Why a server declined, as the application protocol's exception table names it. Any byte may
/// arrive, so a code the table lacks is kept, not refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExceptionCode(pub u8);

impl ExceptionCode {
    pub const ILLEGAL_DATA_ADDRESS: Self = Self(0x02);
    pub const SERVER_DEVICE_FAILURE: Self = Self(0x04);

    /// The request was wrong — function, address or quantity — so the same read fails again: the
    /// driver and the server's register map disagree. Anything else is the server's trouble.
    #[must_use]
    pub const fn is_request_fault(self) -> bool {
        // The first three rows of `name`'s table: function, address, value.
        matches!(self.0, 0x01..=0x03)
    }

    const fn name(self) -> Option<&'static str> {
        match self.0 {
            0x01 => Some("illegal function"),
            0x02 => Some("illegal data address"),
            0x03 => Some("illegal data value"),
            0x04 => Some("server device failure"),
            0x05 => Some("acknowledge"),
            0x06 => Some("server device busy"),
            0x08 => Some("memory parity error"),
            0x0A => Some("gateway path unavailable"),
            0x0B => Some("gateway target device failed to respond"),
            _ => None,
        }
    }
}

impl fmt::Display for ExceptionCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => write!(f, "{name} (0x{:02X})", self.0),
            None => write!(f, "0x{:02X}", self.0),
        }
    }
}
