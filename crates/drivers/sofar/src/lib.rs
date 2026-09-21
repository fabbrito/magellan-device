//! A Sofar inverter, read over Modbus TCP through its LSW-3 logger.
//!
//! One crate holds the whole driver: the wire below, the register map above. They are separate
//! modules and not separate crates, because a second device behind the same logger is what would
//! tell us where the seam between them goes — and there is not one yet.
//!
//! The logger also emits frames in its own protocol on this socket, unasked and unrelated to any
//! read. The decoder recognises and steps over them; nothing here speaks it.

mod error;
mod frame;
mod modbus;
mod session;

pub use crate::error::Error;
pub use crate::frame::{Frame, FrameCodec, ReadRequest, next_frame_tcp};
pub use crate::modbus::registers;
pub use crate::session::{Exchange, Outcome, Session};
