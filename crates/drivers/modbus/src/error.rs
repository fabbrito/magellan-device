//! What a read reply can be instead of registers.

use std::fmt;

#[derive(Debug)]
pub enum ModbusError {
    /// The server declined: an exception reply (unit, function | 0x80, code).
    Exception { function: u8, code: u8 },
    /// A body that is not a well-formed read reply.
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
