//! What can go wrong on the wire.

use std::fmt;
use std::io;

/// Errors produced by the codec.
///
/// Reassembly itself never errors — it consumes ruled-out bytes and resyncs, so a
/// malformed stream costs data, never a panic.
#[derive(Debug)]
pub enum WireError {
    /// The logger closed the connection. Not an I/O failure — the socket ended
    /// cleanly — but the session is over either way.
    Disconnected,
    /// Transport I/O through `Framed`; the
    /// [`Decoder`](tokio_util::codec::Decoder) bound requires it.
    Io(io::Error),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => write!(f, "logger closed the connection"),
            Self::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for WireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Disconnected => None,
        }
    }
}

impl From<io::Error> for WireError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
