//! One read against a node: dial, ask, frame the reply by its MBAP length. What the body means is
//! the caller's.
//!
//! A node is a well-behaved Modbus TCP server, so the length field is trusted to frame. Unlike the
//! Sofar logger, nothing else shares the socket, and a reply that framing cannot follow ends the
//! read rather than being resynced through. A reply to another transaction is discarded and the
//! wait goes on; a direct server's unit id is not significant and is not checked, as the TCP
//! guide has it.

use std::fmt::Display;

use driver::ReadError;
use modbus::{MBAP_PREFIX_SIZE, ReadRequest, mbap_prefix};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio::time::timeout;
use tracing::debug;

use crate::source::Timing;

/// A read the node, or the way to it, turned down — the reason kept for the journal.
pub(crate) fn refused(why: impl Display) -> ReadError {
    ReadError::Refused(why.to_string())
}

/// Dial `address`, send `request`, and return its reply's body: the unit id, then the PDU.
///
/// A connection per read: minutes pass idle between polls, and a held one would go half-open
/// unseen when the node reboots — a known departure from the TCP guide's advice to hold it.
pub async fn read(
    address: impl ToSocketAddrs,
    request: ReadRequest,
    timing: &Timing,
) -> Result<Vec<u8>, ReadError> {
    let mut stream = match timeout(timing.connect, TcpStream::connect(address)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(why)) => {
            debug!(%why, "node did not take the connection");
            return Err(ReadError::Timeout);
        }
        Err(_elapsed) => return Err(ReadError::Timeout),
    };
    timeout(timing.read, read_exchange(&mut stream, request))
        .await
        .map_err(|_elapsed| ReadError::Timeout)?
}

async fn read_exchange(stream: &mut TcpStream, request: ReadRequest) -> Result<Vec<u8>, ReadError> {
    stream.write_all(&request.encode()).await.map_err(refused)?;
    // Ends: the caller's timeout bounds the wait, and every pass consumes a whole frame.
    loop {
        let mut prefix = [0; MBAP_PREFIX_SIZE];
        stream.read_exact(&mut prefix).await.map_err(refused)?;
        let prefix = mbap_prefix(prefix).map_err(refused)?;
        let mut body = vec![0; usize::from(prefix.body_size)];
        stream.read_exact(&mut body).await.map_err(refused)?;
        if prefix.transaction == request.transaction {
            return Ok(body);
        }
        debug!(
            transaction = prefix.transaction,
            "reply to another transaction discarded"
        );
    }
}
