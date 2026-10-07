//! One read against a node: dial, ask, frame the reply by its MBAP length.
//!
//! A node is a well-behaved Modbus TCP server, so the length field is trusted to frame. Unlike the
//! Sofar logger, nothing else shares the socket, and a reply that framing cannot follow ends the
//! read rather than being resynced through.

use driver::ReadError;
use modbus::{MBAP_PREFIX_SIZE, ReadRequest, mbap_prefix, registers};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio::time::timeout;
use tracing::debug;

use crate::source::Timing;

/// Dial `address`, send `request`, and decode its reply.
///
/// A connection per read: minutes pass idle between polls, and a held one would go half-open
/// unseen when the node reboots — a known departure from the TCP guide's advice to hold it.
pub async fn read(
    address: impl ToSocketAddrs,
    request: ReadRequest,
    timing: &Timing,
) -> Result<Vec<u16>, ReadError> {
    let mut stream = match timeout(timing.connect, TcpStream::connect(address)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            debug!(error = %e, "node did not take the connection");
            return Err(ReadError::Timeout);
        }
        Err(_elapsed) => return Err(ReadError::Timeout),
    };
    timeout(timing.read, read_exchange(&mut stream, request))
        .await
        .map_err(|_elapsed| ReadError::Timeout)?
}

async fn read_exchange(
    stream: &mut TcpStream,
    request: ReadRequest,
) -> Result<Vec<u16>, ReadError> {
    let refused = |e: std::io::Error| ReadError::Refused(e.to_string());
    stream.write_all(&request.encode()).await.map_err(refused)?;
    let mut prefix = [0; MBAP_PREFIX_SIZE];
    stream.read_exact(&mut prefix).await.map_err(refused)?;
    let prefix = mbap_prefix(prefix).map_err(|e| ReadError::Refused(e.to_string()))?;
    if prefix.transaction != request.transaction {
        return Err(ReadError::Refused(format!(
            "reply to transaction {}, asked {}",
            prefix.transaction, request.transaction
        )));
    }
    let mut body = vec![0; usize::from(prefix.body_size)];
    stream.read_exact(&mut body).await.map_err(refused)?;
    if body.first() != Some(&request.unit) {
        return Err(ReadError::Refused("reply from another unit".to_owned()));
    }
    registers(&body).map_err(|e| ReadError::Refused(e.to_string()))
}
