//! The wire: Modbus TCP to the logger, and finding the logger on the LAN.

pub(crate) mod discover;
mod error;
pub(crate) mod frame;
pub(crate) mod session;

pub(crate) use crate::wire::error::WireError;
