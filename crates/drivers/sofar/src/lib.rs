//! A Sofar inverter, read over Modbus TCP through its LSW-3 logger.
//!
//! One crate holds the whole driver: the wire below, the register map above. They are separate
//! modules and not separate crates, because a second device behind the same logger is what would
//! tell us where the seam between them goes — and there is not one yet.
//!
//! The logger's own v5 framing was tried first and abandoned: it answered reads in bursts with
//! refusals that plain Modbus TCP never drew. Frames in that shape still land on the socket and
//! the decoder steps over them, but nothing here speaks it.

mod error;
mod frame;
mod modbus;

pub use crate::error::Error;
pub use crate::frame::{Frame, FrameCodec, ReadRequest, next_frame_tcp};
pub use crate::modbus::registers;
