//! Golden vectors: the captured fixtures decode to their embedded values.

use std::fs;

use crate::wire::WireError;
use crate::wire::frame::{Frame, FrameCodec, ReadRequest, next_frame_tcp};
use crate::wire::modbus::registers;
use serde::Deserialize;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::Encoder;

use super::{BoxError, fixture, read_hex};

/// The manifest fields this test consumes. Keys left out here (`note`, `source_ts`) document the
/// capture, they are not test input.
#[derive(Deserialize)]
struct Manifest {
    vector: Vec<Vector>,
}

#[derive(Deserialize)]
struct Vector {
    name: String,
    file: String,
    direction: Direction,
    range: Option<String>,
    seq: Option<u16>,
    fc: Option<Fc>,
    values: Option<Vec<u16>>,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Direction {
    Send,
    Recv,
}

/// `"0x0400-0x042E"` as address and quantity.
fn span(range: &str) -> Result<(u16, u16), BoxError> {
    let (lo, hi) = range.split_once('-').ok_or("range without a dash")?;
    let parse = |h: &str| u16::from_str_radix(h.trim_start_matches("0x"), 16);
    let (lo, hi) = (parse(lo)?, parse(hi)?);
    Ok((lo, hi - lo + 1))
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Fc {
    Holding,
    Input,
}

impl Fc {
    const fn code(&self) -> u8 {
        match self {
            Self::Holding => 3,
            Self::Input => 4,
        }
    }
}

fn vectors() -> Result<Vec<Vector>, BoxError> {
    let text = fs::read_to_string(fixture("manifest.toml"))?;
    Ok(toml::from_str::<Manifest>(&text)?.vector)
}

#[test]
fn recv_vectors_classify_and_decode() {
    let mut replies = 0;
    for v in vectors().unwrap() {
        if v.direction != Direction::Recv {
            continue;
        }
        let mut buf = BytesMut::from(read_hex(&v.file).unwrap().as_slice());
        let frame = next_frame_tcp(&mut buf).unwrap_or_else(|| panic!("{}: no frame", v.name));
        let values = v.values.unwrap_or_else(|| panic!("{}: no values", v.name));
        let fc = v.fc.unwrap_or_else(|| panic!("{}: no fc", v.name));
        let Frame::Reply { rtu, .. } = frame else {
            panic!("{}: expected a reply, got {frame:?}", v.name);
        };
        assert_eq!(
            rtu.get(1).copied(),
            Some(fc.code()),
            "{}: fc mismatch",
            v.name
        );
        let got = registers(&rtu).unwrap_or_else(|e| panic!("{}: {e}", v.name));
        assert_eq!(got, values, "{}: value mismatch", v.name);
        assert!(buf.is_empty(), "{}: trailing bytes after one frame", v.name);
        replies += 1;
    }
    // A manifest that lost its recv vectors would pass every check above vacuously.
    assert!(replies > 0, "no reply vector checked");
}

#[test]
fn requests_rebuild_and_their_replies_echo_the_txn() {
    let vectors = vectors().unwrap();
    let mut checked = 0;
    for v in vectors.iter().filter(|v| v.direction == Direction::Send) {
        let (addr, qty) = span(v.range.as_deref().unwrap()).unwrap();
        let seq = v.seq.unwrap_or_else(|| panic!("{}: no seq", v.name));
        let request = ReadRequest {
            txn: seq,
            slave: 1,
            fc: 3,
            addr,
            qty,
        };
        let mut built = BytesMut::new();
        FrameCodec::new().encode(request, &mut built).unwrap();
        assert_eq!(built.as_ref(), read_hex(&v.file).unwrap(), "{}", v.name);

        let reply = vectors
            .iter()
            .find(|r| r.direction == Direction::Recv && r.range == v.range)
            .unwrap_or_else(|| panic!("{}: no reply", v.name));
        let echoed = read_hex(&reply.file).unwrap();
        assert_eq!(
            echoed.get(..2),
            Some(seq.to_be_bytes().as_slice()),
            "{}",
            reply.name
        );
        checked += 1;
    }
    assert!(checked > 0, "no request vector checked");
}

#[test]
fn a_modbus_exception_is_a_refusal_that_registers_can_name() {
    // Constructed, not captured: no refusal is among the fixtures. The path still has to hold —
    // an exception is a refusal at the frame layer and never reaches decode as data.
    let mut buf = BytesMut::from([0x12, 0x34, 0x00, 0x00, 0x00, 0x03, 0x01, 0x83, 0x02].as_slice());
    let frame = next_frame_tcp(&mut buf).expect("an exception frame");
    let Frame::Refusal { raw } = frame else {
        panic!("expected a refusal, got {frame:?}");
    };
    // Unit id onward is the body, as the session would hand it on.
    match registers(&raw[6..]) {
        Err(WireError::ModbusException { fc, code }) => assert_eq!((fc, code), (3, 2)),
        other => panic!("expected ModbusException, got {other:?}"),
    }
}
