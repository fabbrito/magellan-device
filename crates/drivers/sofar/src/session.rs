//! One connection to the logger, and the discipline it demands.
//!
//! The logger is shared: the vendor's cloud holds a connection, other clients may too, and how
//! many it will grant is undocumented. Three rules it enforces rather than documents:
//!
//! - **The transaction id never repeats.** It is seeded per connection from the clock and advances
//!   with every read. A repeated number is one the logger has already answered.
//! - **The connection is released, always.** `SO_LINGER 0` is set before the first byte, so
//!   closing sends RST and the logger forgets it at once. A graceful close leaves the entry in
//!   the logger's table for minutes, and abandoned ones pile up in a table of unknown size.
//! - **Reads are strictly serial.** One read is in flight at a time, and a reply whose
//!   transaction id is not the one in flight belongs to an earlier read — discarded, never taken
//!   for the answer.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use socket2::SockRef;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tokio_util::bytes::{Bytes, BytesMut};
use tokio_util::codec::{Encoder, Framed};

use crate::error::Error;
use crate::frame::{Frame, FrameCodec, ReadRequest};

/// Closing must free the connection immediately, not linger in the logger's
/// table.
/// Set through socket2: tokio deprecated its own setter because a non-zero
/// linger blocks the thread on drop, which is the case this is not.
const LINGER: Duration = Duration::ZERO;

/// What one read produced.
#[derive(Debug)]
pub enum Outcome {
    /// A read reply: `rtu` is the Modbus body.
    Reply { rtu: Bytes },
    /// The logger answered with its failure body instead of data. What it
    /// means is unknown; it is final for this read.
    Refusal,
    /// Nothing arrived in time.
    TimedOut,
    /// The connection died after the request went out. The request is still a
    /// fact to record; the connection is not.
    Lost(Error),
}

/// One request and whatever came back.
#[derive(Debug)]
pub struct Exchange {
    pub outcome: Outcome,
    pub elapsed: Duration,
}

/// A live connection to the logger.
#[derive(Debug)]
pub struct Session {
    slave: u8,
    txn: u16,
    conn: Framed<TcpStream, FrameCodec>,
}

impl Session {
    /// Open a connection to the logger.
    ///
    /// # Errors
    ///
    /// If the address cannot be reached within `limit` or `SO_LINGER` cannot be
    /// set — without the latter a close would linger in the logger, so it is fatal
    /// rather than ignored.
    ///
    /// `limit` is not optional in practice: connecting to an address that has
    /// gone dark blocks for the operating system's own timeout, minutes during
    /// which nothing is recorded and no backoff runs.
    pub async fn connect(addr: &str, slave: u8, limit: Duration) -> Result<Self, Error> {
        let conn = Self::dial(addr, limit).await?;
        Ok(Self {
            slave,
            txn: seed_txn(),
            conn,
        })
    }

    async fn dial(addr: &str, limit: Duration) -> Result<Framed<TcpStream, FrameCodec>, Error> {
        let stream = timeout(limit, TcpStream::connect(addr))
            .await
            .map_err(|_| Error::Disconnected)??;
        SockRef::from(&stream).set_linger(Some(LINGER))?;
        Ok(Framed::new(stream, FrameCodec::new()))
    }

    /// Read `qty` registers from `addr`, waiting at most `limit` for an answer.
    ///
    /// Heartbeat frames arriving mid-wait are skipped: they are unsolicited and
    /// are never the answer to this request.
    ///
    /// # Errors
    ///
    /// Only when nothing went out: the request could not be encoded or written.
    /// Once it is on the wire, a dead connection comes back as
    /// [`Outcome::Lost`] and a silent one as [`Outcome::TimedOut`], so the
    /// request that preceded either is never lost with it.
    pub async fn read(&mut self, addr: u16, qty: u16, limit: Duration) -> Result<Exchange, Error> {
        self.txn = self.txn.wrapping_add(1);
        let txn = self.txn;
        let mut sent = BytesMut::new();
        FrameCodec::new().encode(
            ReadRequest {
                txn,
                slave: self.slave,
                fc: 3,
                addr,
                qty,
            },
            &mut sent,
        )?;
        let started = Instant::now();
        self.conn.get_mut().write_all(&sent).await?;
        let outcome = match timeout(limit, self.await_reply(txn)).await {
            Err(_elapsed) => Outcome::TimedOut,
            Ok(Ok(outcome)) => outcome,
            Ok(Err(e)) => Outcome::Lost(e),
        };
        Ok(Exchange {
            outcome,
            elapsed: started.elapsed(),
        })
    }

    /// Wait for the frame that answers this read, skipping heartbeats.
    ///
    /// A reply whose txn is not the one we sent belongs to an earlier read — a late arrival
    /// after a timeout — and is discarded, not taken for the answer. The scheduler never reads a
    /// timed-out socket again, but the API has to survive a caller that does.
    async fn await_reply(&mut self, txn: u16) -> Result<Outcome, Error> {
        loop {
            match self.conn.next().await {
                None => return Err(Error::Disconnected),
                Some(Err(e)) => return Err(e),
                Some(Ok(Frame::Reply { raw, rtu })) => {
                    if !txn_echoes(&raw, txn) {
                        continue;
                    }
                    return Ok(Outcome::Reply { rtu });
                }
                Some(Ok(Frame::Refusal { raw })) => {
                    if !txn_echoes(&raw, txn) {
                        continue;
                    }
                    return Ok(Outcome::Refusal);
                }
            }
        }
    }
}

/// A starting transaction id the logger is unlikely to have seen recently.
///
/// Clock nanoseconds rather than a random-number dependency: the only property
/// needed is that two runs a moment apart do not start at the same number.
fn seed_txn() -> u16 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |since| since.subsec_nanos() as u16)
}

/// True when the reply's transaction id echoes the one the request carried.
fn txn_echoes(raw: &Bytes, txn: u16) -> bool {
    raw.get(0..2)
        .and_then(|b| <[u8; 2]>::try_from(b).ok())
        .is_some_and(|b| u16::from_be_bytes(b) == txn)
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;
    use tokio::time::sleep;

    use super::*;
    use crate::frame::tests::counter_frame;
    use crate::modbus::registers;

    /// A captured reply to a ten-register read at 0x0580.
    const REPLY: &str = include_str!("captures/fixtures/tcp-range-0580.hex");
    /// txn, protocol, length, unit, then the five-byte PDU. Every request is this long.
    const REQUEST_LEN: usize = 12;
    const LIMIT: Duration = Duration::from_millis(250);
    /// Outlasts the kernel's first SYN retransmit, which a dial into a full accept queue waits
    /// for.
    const AMPLE: Duration = Duration::from_secs(5);

    /// What the fake logger does when a request arrives.
    enum Act {
        /// Wrap `body` in an MBAP reply echoing the request's txn, after `prefix`. The logger has
        /// to echo to answer at all: nothing else knows the txn before it has seen the request.
        EchoTxn { prefix: Vec<u8>, body: Vec<u8> },
        /// Like [`Act::EchoTxn`], but `delay` after the request arrived — a reply that lands
        /// after the caller's timeout.
        EchoTxnLate {
            prefix: Vec<u8>,
            body: Vec<u8>,
            delay: Duration,
        },
        /// Accept the request and never answer it.
        Silence,
        /// Drop the connection mid-session.
        Close,
    }

    fn hex(text: &str) -> Vec<u8> {
        text.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).expect("fixture is hex"))
            .collect()
    }

    /// The captured reply's PDU — function code, byte count, data — which a fresh MBAP header
    /// wraps once the txn is known.
    fn reply_body() -> Vec<u8> {
        hex(REPLY)[7..].to_vec()
    }

    /// An MBAP reply echoing the request's txn, `body` as the PDU.
    fn txn_reply(request: &[u8], prefix: &[u8], body: &[u8]) -> Vec<u8> {
        let txn = [request[0], request[1]];
        let mut reply = prefix.to_vec();
        reply.extend_from_slice(&txn);
        reply.extend_from_slice(&[0, 0]);
        reply.extend_from_slice(&u16::try_from(body.len() + 1).unwrap().to_be_bytes());
        reply.push(1); // unit
        reply.extend_from_slice(body);
        reply
    }

    /// A stand-in for the logger: accepts connections and plays `script`, one act per request.
    /// The script survives reconnects, so a test can assert what the second connection sends.
    async fn fake_logger(script: Vec<Act>) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let handle = tokio::spawn(async move {
            let mut script = script.into_iter();
            while let Ok((mut sock, _)) = listener.accept().await {
                loop {
                    let mut request = vec![0u8; REQUEST_LEN];
                    if sock.read_exact(&mut request).await.is_err() {
                        break;
                    }
                    match script.next() {
                        Some(Act::EchoTxn { prefix, body }) => {
                            if sock
                                .write_all(&txn_reply(&request, &prefix, &body))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Some(Act::EchoTxnLate {
                            prefix,
                            body,
                            delay,
                        }) => {
                            sleep(delay).await;
                            if sock
                                .write_all(&txn_reply(&request, &prefix, &body))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Some(Act::Silence) => {}
                        Some(Act::Close) | None => break,
                    }
                }
            }
        });
        (addr, handle)
    }

    async fn session(script: Vec<Act>) -> Session {
        let (addr, _logger) = fake_logger(script).await;
        Session::connect(&addr, 1, LIMIT).await.expect("connect")
    }

    /// The whole reply the fixture captured, rewrapped with whatever txn the request carried.
    fn echo_reply() -> Act {
        Act::EchoTxn {
            prefix: vec![],
            body: reply_body(),
        }
    }

    #[tokio::test]
    async fn a_read_returns_the_reply_to_the_request_it_sent() {
        let mut s = session(vec![echo_reply()]).await;
        let exchange = s.read(0x0580, 10, LIMIT).await.expect("read");
        let Outcome::Reply { rtu, .. } = exchange.outcome else {
            panic!("expected a reply");
        };
        assert_eq!(registers(&rtu).expect("decodes").len(), 10);
        // That the request's bytes are the captured ones is `codec_vectors`; what this proves is
        // that a reply reaches the caller paired with the id it was asked under.
    }

    #[tokio::test]
    async fn an_exception_is_a_refusal_that_echoes_the_txn() {
        let mut s = session(vec![Act::EchoTxn {
            prefix: vec![],
            body: vec![0x83, 0x02],
        }])
        .await;
        let exchange = s.read(0x0580, 10, LIMIT).await.expect("read");
        assert!(matches!(exchange.outcome, Outcome::Refusal));
    }

    #[tokio::test]
    async fn a_logger_frame_arriving_first_is_not_mistaken_for_the_answer() {
        let mut s = session(vec![Act::EchoTxn {
            prefix: counter_frame(),
            body: reply_body(),
        }])
        .await;
        let exchange = s.read(0x0580, 10, LIMIT).await.expect("read");
        assert!(matches!(exchange.outcome, Outcome::Reply { .. }));
    }

    #[tokio::test]
    async fn a_reply_with_the_wrong_txn_is_not_taken_for_the_answer() {
        // A reply for an earlier read arriving after its timeout must not be handed to the next
        // read: the txn gives it away, and it is discarded.
        let mut s = session(vec![
            Act::EchoTxnLate {
                prefix: vec![],
                body: vec![0x03, 0x02, 0x12, 0x34], // one register, 0x1234
                delay: Duration::from_millis(300),  // past the 250 ms limit
            },
            echo_reply(),
        ])
        .await;
        let first = s.read(0x0580, 10, LIMIT).await.expect("read");
        assert!(matches!(first.outcome, Outcome::TimedOut));
        // The late reply is in the buffer when the next read waits; its txn is the first read's,
        // so it must be skipped, not accepted as the answer.
        let second = s.read(0x0580, 10, LIMIT).await.expect("read");
        let Outcome::Reply { rtu, .. } = second.outcome else {
            panic!("expected the second reply, not the stale one");
        };
        assert_eq!(
            registers(&rtu).expect("decodes").len(),
            10,
            "the stale one-register reply must be discarded"
        );
    }

    #[tokio::test]
    async fn a_timeout_leaves_the_connection_usable() {
        let mut s = session(vec![Act::Silence, echo_reply()]).await;
        let first = s
            .read(0x0580, 10, LIMIT)
            .await
            .expect("no error on timeout");
        assert!(matches!(first.outcome, Outcome::TimedOut));
        // A timeout is a fact about one read, not about the connection: spending the slot on a
        // reconnect here would be the expensive mistake.
        let second = s.read(0x0580, 10, LIMIT).await.expect("still connected");
        assert!(matches!(second.outcome, Outcome::Reply { .. }));
    }

    #[tokio::test]
    async fn a_connection_closed_after_the_request_is_lost_with_the_request_kept() {
        let mut s = session(vec![Act::Close]).await;
        // The logger hung up after reading the request: that is an outcome of a read that went
        // out, not an error that swallows what was sent.
        let exchange = s
            .read(0x0580, 10, LIMIT)
            .await
            .expect("the request went out");
        assert!(
            matches!(exchange.outcome, Outcome::Lost(_)),
            "{:?}",
            exchange.outcome
        );
    }

    #[tokio::test]
    async fn the_transaction_id_advances_with_every_read() {
        // A repeated number is one the logger has already answered.
        let mut s = session(vec![echo_reply(), echo_reply()]).await;
        let seeded = s.txn;
        s.read(0x0580, 10, LIMIT).await.expect("read");
        assert_eq!(s.txn, seeded.wrapping_add(1));
        s.read(0x0580, 10, LIMIT).await.expect("read");
        assert_eq!(s.txn, seeded.wrapping_add(2));
    }

    #[tokio::test]
    async fn the_connection_closes_without_lingering() {
        // Proves the option is on the socket, not that the logger's table clears faster — nothing
        // offline can show that. It is here because the rule is invisible otherwise: every test
        // below passes with the line deleted.
        let (addr, _logger) = fake_logger(vec![]).await;
        let conn = Session::dial(&addr, AMPLE).await.expect("dial");
        let linger = SockRef::from(conn.get_ref())
            .linger()
            .expect("linger is readable");
        assert_eq!(
            linger,
            Some(LINGER),
            "SO_LINGER 0 must be set before the first byte"
        );
    }
}
