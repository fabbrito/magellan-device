//! The node as a source the runtime polls.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use contract::{Metric, Reading};
use driver::{ReadError, Source};
use modbus::{ReadFunction, ReadRequest};
use tracing::debug;

use crate::{discover, wire};

/// The register map's span: one read covers it.
const REGISTER_FIRST: u16 = 0;
const REGISTER_COUNT: u16 = 7;
/// A firmware part at or past this cannot pack: it would spill into the part above.
const FIRMWARE_PART_LIMIT: u16 = 1000;

const _: () = assert!(REGISTER_COUNT <= modbus::QUANTITY_MAX);
// The largest packed version stays inside what the contract carries.
const _: () =
    assert!(u16::MAX as i64 * 1_000_000 + 999 * 1000 + 999 <= contract::limits::METRIC_VALUE_MAX);

/// How long the driver waits, at each step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Longest a dial may take before the node counts as dark.
    pub connect: Duration,
    /// Longest the read may take, once connected.
    pub read: Duration,
    /// How long the node gets to answer over mDNS.
    pub discovery: Duration,
}

/// Where the node is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Locate {
    /// A configured `host:port`, dialled as is.
    Host(String),
    /// Found by its advertised id when a poll needs it; forgotten when a read through it fails,
    /// so a node that took a new lease is found again on the next poll.
    Discover {
        node_id: String,
        found: Option<SocketAddr>,
    },
}

/// One node.
#[derive(Debug)]
pub struct Node {
    id: String,
    metrics: Vec<Metric>,
    locate: Locate,
    unit: u8,
    timing: Timing,
    transaction: u16,
}

impl Node {
    pub(crate) fn new(id: String, locate: Locate, unit: u8, timing: Timing) -> Self {
        Self {
            id,
            metrics: metrics(),
            locate,
            unit,
            timing,
            transaction: 0,
        }
    }

    #[cfg(test)]
    pub(crate) const fn locate(&self) -> &Locate {
        &self.locate
    }

    #[cfg(test)]
    pub(crate) const fn timing(&self) -> Timing {
        self.timing
    }

    async fn read_registers(&mut self) -> Result<Vec<u16>, ReadError> {
        self.transaction = self.transaction.wrapping_add(1);
        let request = ReadRequest {
            transaction: self.transaction,
            unit: self.unit,
            function: ReadFunction::Input,
            address: REGISTER_FIRST,
            quantity: REGISTER_COUNT,
        };
        match &mut self.locate {
            Locate::Host(host) => wire::read(host.as_str(), request, &self.timing).await,
            Locate::Discover { node_id, found } => {
                let address = match found {
                    Some(address) => *address,
                    None => discover::find(node_id, self.timing.discovery).await?,
                };
                let words = wire::read(address, request, &self.timing).await;
                *found = words.is_ok().then_some(address);
                words
            }
        }
    }
}

#[async_trait]
impl Source for Node {
    fn id(&self) -> &str {
        &self.id
    }

    fn metrics(&self) -> &[Metric] {
        &self.metrics
    }

    async fn read(&mut self, timestamp_ms: u64) -> Result<Reading, ReadError> {
        let words = self.read_registers().await.inspect_err(|e| {
            debug!(source = %self.id, error = ?e, "node read failed");
        })?;
        Ok(Reading {
            source: self.id.clone(),
            ts: timestamp_ms,
            values: values(&words)?,
        })
    }
}

fn metrics() -> Vec<Metric> {
    vec![
        Metric::Counter {
            key: "uptime".to_owned(),
            unit: Some("s".to_owned()),
            exponent: 0,
            resets: None,
        },
        Metric::Gauge {
            key: "rssi".to_owned(),
            unit: Some("dBm".to_owned()),
            exponent: 0,
        },
        Metric::Gauge {
            key: "random".to_owned(),
            unit: None,
            exponent: 0,
        },
        Metric::State {
            key: "firmware".to_owned(),
            state_labels: None,
        },
    ]
}

/// The register map's words as metric values.
fn values(words: &[u16]) -> Result<BTreeMap<String, i64>, ReadError> {
    let &[uptime_high, uptime_low, rssi, random, major, minor, patch] = words else {
        return Err(ReadError::Refused(format!(
            "{} registers back, asked {REGISTER_COUNT}",
            words.len()
        )));
    };
    if minor >= FIRMWARE_PART_LIMIT || patch >= FIRMWARE_PART_LIMIT {
        return Err(ReadError::Refused(format!(
            "firmware {major}.{minor}.{patch} does not pack"
        )));
    }
    let uptime = (u32::from(uptime_high) << 16) | u32::from(uptime_low);
    let firmware = i64::from(major) * 1_000_000 + i64::from(minor) * 1000 + i64::from(patch);
    Ok(BTreeMap::from([
        ("uptime".to_owned(), i64::from(uptime)),
        (
            "rssi".to_owned(),
            i64::from(i16::from_be_bytes(rssi.to_be_bytes())),
        ),
        ("random".to_owned(), i64::from(random)),
        ("firmware".to_owned(), firmware),
    ]))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    const WORDS: [u16; 7] = [0x0001, 0x0002, 0xffb5, 42, 1, 2, 3];
    const AMPLE: Duration = Duration::from_secs(5);

    /// Where a read request carries what a reply echoes.
    const REQUEST_UNIT: usize = 6;
    const REQUEST_FUNCTION: usize = 7;

    fn transaction_of(request: &[u8]) -> [u8; 2] {
        [request[0], request[1]]
    }

    /// The PDU of a reply carrying `words` as input registers.
    fn words_pdu(words: &[u16]) -> Vec<u8> {
        let count = u8::try_from(words.len() * 2).unwrap();
        let mut pdu = vec![ReadFunction::Input.code(), count];
        for word in words {
            pdu.extend_from_slice(&word.to_be_bytes());
        }
        pdu
    }

    /// `pdu` behind an MBAP header for `transaction` and `unit`.
    fn framed(transaction: [u8; 2], unit: u8, pdu: &[u8]) -> Vec<u8> {
        let [length_high, length_low] = u16::try_from(pdu.len() + 1).unwrap().to_be_bytes();
        let [transaction_high, transaction_low] = transaction;
        let mut frame = vec![
            transaction_high,
            transaction_low,
            0,
            0,
            length_high,
            length_low,
            unit,
        ];
        frame.extend_from_slice(pdu);
        frame
    }

    /// A node answering one input-register read with what `reply` builds from the request.
    async fn fake_node_replying(reply: impl FnOnce(&[u8]) -> Vec<u8> + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; modbus::READ_REQUEST_SIZE];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(
                request[REQUEST_FUNCTION],
                ReadFunction::Input.code(),
                "reads input registers"
            );
            stream.write_all(&reply(&request)).await.unwrap();
        });
        address
    }

    /// A node answering one read with `words`, echoing the request's transaction and unit.
    async fn fake_node(words: &'static [u16]) -> String {
        fake_node_replying(|request| {
            framed(
                transaction_of(request),
                request[REQUEST_UNIT],
                &words_pdu(words),
            )
        })
        .await
    }

    fn node_at(address: String) -> Node {
        let timing = Timing {
            connect: AMPLE,
            read: AMPLE,
            discovery: AMPLE,
        };
        Node::new("garage".to_owned(), Locate::Host(address), 1, timing)
    }

    #[tokio::test]
    async fn a_node_reads_as_its_register_map() {
        let mut node = node_at(fake_node(&WORDS).await);
        let reading = node.read(1_700_000_000_000).await.unwrap();
        assert_eq!(reading.source, "garage");
        assert_eq!(reading.ts, 1_700_000_000_000);
        assert_eq!(
            reading.values,
            BTreeMap::from([
                ("uptime".to_owned(), 0x0001_0002),
                ("rssi".to_owned(), -75),
                ("random".to_owned(), 42),
                ("firmware".to_owned(), 1_002_003),
            ])
        );
    }

    #[tokio::test]
    async fn a_short_answer_is_refused() {
        let mut node = node_at(fake_node(&[1, 2, 3]).await);
        assert!(matches!(node.read(0).await, Err(ReadError::Refused(_))));
    }

    #[tokio::test]
    async fn a_dark_node_is_a_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        drop(listener);
        assert!(matches!(
            node_at(address).read(0).await,
            Err(ReadError::Timeout)
        ));
    }

    #[test]
    fn every_value_read_is_a_metric_declared() {
        let declared = metrics()
            .iter()
            .map(|metric| metric.key().to_owned())
            .collect::<BTreeSet<_>>();
        let read = values(&WORDS).unwrap().into_keys().collect::<BTreeSet<_>>();
        assert_eq!(read, declared);
    }

    #[test]
    fn a_firmware_part_that_would_spill_is_refused() {
        let words = [0, 0, 0, 0, 1, 1000, 0];
        assert!(matches!(values(&words), Err(ReadError::Refused(_))));
    }

    #[test]
    fn the_manifest_the_node_declares_holds_to_the_contract() {
        for metric in metrics() {
            assert!(metric.validate().is_ok(), "{metric:?}");
        }
    }
}
