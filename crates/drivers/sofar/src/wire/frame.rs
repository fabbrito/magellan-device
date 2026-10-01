//! Frame assembly and reassembly for Modbus TCP.
//!
//! MBAP has no start marker and carries no Modbus CRC, so nothing confirms a frame boundary after
//! the fact. A candidate header is judged by its fields instead — protocol id `00 00`, a length
//! inside Modbus bounds that agrees with the function code and byte count, and a body that passes
//! the structural check. What fails is consumed one byte at a time and the search resumes.
//!
//! The logger does not only answer reads. Unasked, it puts frames of its own protocol on the same
//! socket — Solarman v5: `0xA5` start, `0x15` end, `0x4710` counters among them. None is ever the
//! answer to a read, so a complete one is stepped over whole. That is what the v5 constants below
//! are for, and the only thing they are for.

use tokio_util::bytes::{Buf, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::wire::WireError;

const START: u8 = 0xA5;
const END: u8 = 0x15;
const CONTROL_RESPONSE: u16 = 0x1510;
const CONTROL_COUNTER: u16 = 0x4710;
/// Standard-shape frame bytes the length field does not count.
const HEADER_TRAILER: usize = 13;
/// The only read function code in play. Narrow deliberately: this check stands
/// in for the CRC the short shape drops, so every code it accepts is a false
/// boundary it can no longer reject.
const READ_FC: u8 = 3;
/// MBAP header: transaction id, protocol id, length. Unit id and PDU follow.
const MBAP_HEADER: usize = 6;
/// Largest length field a reply can carry: unit id plus a 253-byte PDU.
const MBAP_LEN_MAX: u16 = 254;
/// Largest length a standard-shaped v5 frame can claim: 23 fixed bytes
/// (control, seq, serial, type, sensor) plus a 254-byte PDU, with margin. A
/// claim beyond this is a false head, not a frame that may still complete.
const V5_LEN_MAX: u16 = 0x0120;
/// Modbus TCP requires protocol id `00 00`; anything else is not an MBAP frame.
const MBAP_PROTOCOL_ID: u16 = 0;

/// One complete frame off the wire.
#[derive(Debug)]
pub enum Frame {
    /// A sane Modbus read reply (FC3/4), RTU without any Modbus CRC.
    Reply { raw: Bytes, rtu: Bytes },
    /// A failure body, not data: a Modbus exception reply, or a body that is
    /// not a well-formed one.
    Refusal { raw: Bytes },
}

/// The outbound read request the [`Encoder`] serialises.
#[derive(Debug)]
pub struct ReadRequest {
    /// The MBAP transaction id a reply must echo. The session owns the counter and never repeats
    /// it.
    pub txn: u16,
    pub slave: u8,
    pub fc: u8,
    pub addr: u16,
    pub qty: u16,
}

/// Sort a frame by its body: a read reply, or the logger refusing to answer.
fn classify(raw: Bytes, rtu: Bytes) -> Frame {
    if rtu.get(1).is_some_and(|&fc| fc & 0x80 != 0) {
        // A Modbus exception (fc | 0x80) is the logger declining to answer, not
        // data. It tallies as a refusal and never reaches decode.
        Frame::Refusal { raw }
    } else if rtu_is_sane(&rtu) {
        Frame::Reply { raw, rtu }
    } else {
        Frame::Refusal { raw }
    }
}

/// What the head of the buffer is, as far as the logger's own protocol goes.
enum LoggerFrame {
    /// One was stepped over whole, or a false head was resynced past. Look again.
    Consumed,
    /// One has begun and not all of it has arrived.
    Incomplete,
    /// The head is not the logger's.
    Absent,
}

/// Step over a frame in the logger's own protocol — a `0x4710` counter, or a
/// `0x1510` reply shape — which is never the answer to a read.
///
/// Whole-frame skipping beats resyncing a byte at a time, which would search
/// the frame's body for an MBAP header and could find a false one.
fn skip_logger_frame(buf: &mut BytesMut) -> LoggerFrame {
    let Some(control) = buf.get(3..5).and_then(|c| <[u8; 2]>::try_from(c).ok()) else {
        return LoggerFrame::Absent;
    };
    if buf.first() != Some(&START)
        || !matches!(
            u16::from_le_bytes(control),
            CONTROL_COUNTER | CONTROL_RESPONSE
        )
    {
        return LoggerFrame::Absent;
    }
    let Some(len) = buf.get(1..3).and_then(|l| <[u8; 2]>::try_from(l).ok()) else {
        return LoggerFrame::Incomplete;
    };
    let len = usize::from(u16::from_le_bytes(len));
    if len > usize::from(V5_LEN_MAX) {
        buf.advance(1); // the length field is absurd — a false v5 head
        return LoggerFrame::Consumed;
    }
    let total = HEADER_TRAILER + len;
    if buf.len() < total {
        return LoggerFrame::Incomplete;
    }
    if buf.get(total - 1).copied() == Some(END) {
        buf.advance(total); // complete, and not our answer — skip whole
    } else {
        buf.advance(1); // the length field lied — resync one byte
    }
    LoggerFrame::Consumed
}

/// Consume and return the next complete Modbus TCP frame, or `None`.
///
/// Never errors, and consumes what it rules out, so a caller can keep appending
/// bytes and call again. MBAP has no start marker, so a candidate header is
/// judged by its fields — protocol id `00 00`, a length within Modbus bounds
/// that agrees with the function code and byte count, and a body that passes
/// [`rtu_is_sane`]. The logger's own frames arrive on this socket unasked; a
/// complete one is stepped over whole.
pub fn next_frame_tcp(buf: &mut BytesMut) -> Option<Frame> {
    loop {
        match skip_logger_frame(buf) {
            LoggerFrame::Consumed => continue,
            LoggerFrame::Incomplete => return None,
            LoggerFrame::Absent => {}
        }
        if buf.len() < MBAP_HEADER {
            return None;
        }
        let protocol = u16::from_be_bytes(buf.get(2..4)?.try_into().ok()?);
        if protocol != MBAP_PROTOCOL_ID {
            buf.advance(1); // not an MBAP header — resync
            continue;
        }
        let len = u16::from_be_bytes(buf.get(4..6)?.try_into().ok()?);
        if len == 0 || len > MBAP_LEN_MAX {
            buf.advance(1); // the length field is absurd — resync
            continue;
        }
        // The function code and byte count are visible at 9 bytes. A header
        // they contradict can never complete; rule it out now instead of
        // waiting out the read — the truncated 8–10 byte replies the logger is
        // reported to send are the likely trigger.
        if buf.len() >= 9
            && let (Some(&fc), Some(&count)) = (buf.get(7), buf.get(8))
        {
            let sane = if fc & 0x80 != 0 {
                len == 3 // an exception: unit, fc|0x80, code
            } else {
                fc == READ_FC && len == 3 + u16::from(count)
            };
            if !sane {
                buf.advance(1); // the header contradicts the body — resync
                continue;
            }
        }
        let total = MBAP_HEADER + usize::from(len);
        if buf.len() < total {
            return None;
        }
        let mut rtu = BytesMut::with_capacity(usize::from(len));
        rtu.extend_from_slice(buf.get(6..total)?); // unit id + PDU, no CRC
        if !rtu_is_sane(&rtu) {
            buf.advance(1); // a false header — resync
            continue;
        }
        let raw = buf.split_to(total).freeze();
        let rtu = rtu.freeze();
        return Some(classify(raw, rtu));
    }
}

/// Structural check standing in for the CRC the short shape strips.
///
/// A read reply carries its own byte count, so length and content have to agree —
/// enough to tell a real reply from the logger's failure body, and to reject a
/// false `0xA5` boundary without a checksum to lean on.
fn rtu_is_sane(rtu: &[u8]) -> bool {
    let Some(&fc) = rtu.get(1) else {
        return false;
    };
    if fc & 0x80 != 0 {
        // Modbus exception reply: slave, fc|0x80, code. Sane — and a refusal,
        // counted as one, never decoded.
        return rtu.len() == 3 && fc & 0x7F == READ_FC;
    }
    rtu.len() >= 3
        && fc == READ_FC
        && u8::try_from(rtu.len() - 3).is_ok_and(|count| rtu.get(2) == Some(&count))
}

/// The [`Decoder`]/[`Encoder`] pair for `Framed`.
///
/// Modbus TCP, which is the whole of what this driver speaks. The logger's own frames arrive on
/// the socket unasked, so the decoder recognises and steps over them.
#[derive(Debug, Default, Clone, Copy)]
pub struct FrameCodec;

impl FrameCodec {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = WireError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, WireError> {
        Ok(next_frame_tcp(src))
    }
}

impl Encoder<ReadRequest> for FrameCodec {
    type Error = WireError;

    fn encode(&mut self, item: ReadRequest, dst: &mut BytesMut) -> Result<(), WireError> {
        // txn(2 BE) protocol(2) length(2) unit fc addr(2 BE) qty(2 BE).
        // The length is fixed: the unit id plus the 5-byte PDU.
        dst.extend_from_slice(&item.txn.to_be_bytes());
        dst.extend_from_slice(&[0, 0, 0, 6]);
        dst.extend_from_slice(&[item.slave, item.fc]);
        dst.extend_from_slice(&item.addr.to_be_bytes());
        dst.extend_from_slice(&item.qty.to_be_bytes());
        Ok(())
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::wire::modbus::registers;

    /// The 20 data bytes of the range-0580 fixture, the known-good reply.
    const DATA: [u8; 20] = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xff, 0x09, 0xf1, 0x02, 0x11, 0x00, 0x86, 0x09,
        0xe2, 0x02, 0x20, 0x00, 0x89,
    ];

    /// A known-good FC3 reply PDU: fc, byte count, then the data.
    fn reply_pdu() -> Vec<u8> {
        let mut pdu = vec![0x03, 0x14];
        pdu.extend_from_slice(&DATA);
        pdu
    }

    /// Wrap `pdu` in an MBAP frame echoing `txn`, unit id 1.
    fn tcp_frame(txn: u16, pdu: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(7 + pdu.len());
        out.extend_from_slice(&txn.to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&u16::try_from(pdu.len() + 1).unwrap().to_be_bytes());
        out.push(1);
        out.extend_from_slice(pdu);
        out
    }

    /// Standard-shaped heartbeat (control 0x4710, 15-byte payload). Hand-built
    /// — no capture holds one, they carry no reply.
    pub fn counter_frame() -> Vec<u8> {
        let mut frame = vec![
            0xa5, 0x0f, 0x00, // len = 15
            0x10, 0x47, // control 0x4710
            0x01, 0x00, // seq
            0xef, 0xbe, 0xad, 0xde, // serial
            0x02,
        ];
        frame.extend_from_slice(&[0u8; 14]);
        let checksum = frame
            .iter()
            .skip(1)
            .fold(0u8, |acc, &b| acc.wrapping_add(b));
        frame.push(checksum);
        frame.push(0x15);
        frame
    }

    #[test]
    fn a_modbus_tcp_request_is_exactly_txn_header_unit_pdu() {
        let mut codec = FrameCodec::new();
        let mut dst = BytesMut::new();
        codec
            .encode(
                ReadRequest {
                    txn: 0x1234,
                    slave: 1,
                    fc: 3,
                    addr: 0x0580,
                    qty: 10,
                },
                &mut dst,
            )
            .expect("encodes");
        assert_eq!(
            dst.as_ref(),
            [
                0x12, 0x34, 0x00, 0x00, 0x00, 0x06, 0x01, 0x03, 0x05, 0x80, 0x00, 0x0a
            ]
        );
    }

    #[test]
    fn a_modbus_tcp_reply_decodes_through_registers() {
        let mut buf = BytesMut::from(&tcp_frame(0x1234, &reply_pdu())[..]);
        let Frame::Reply { raw, rtu } = next_frame_tcp(&mut buf).expect("a reply") else {
            panic!("expected a reply");
        };
        assert_eq!(registers(&rtu).expect("decodes").len(), 10);
        assert_eq!(u16::from_be_bytes([raw[0], raw[1]]), 0x1234, "txn echoed");
        assert!(buf.is_empty(), "one frame consumed exactly");
    }

    #[test]
    fn a_modbus_tcp_exception_is_a_refusal() {
        let mut buf = BytesMut::from(&tcp_frame(0x1234, &[0x83, 0x02])[..]);
        let frame = next_frame_tcp(&mut buf).expect("an exception");
        assert!(matches!(frame, Frame::Refusal { .. }));
    }

    #[test]
    fn a_heartbeat_between_modbus_tcp_frames_is_skipped() {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&counter_frame());
        buf.extend_from_slice(&tcp_frame(0x1234, &reply_pdu()));
        let frame = next_frame_tcp(&mut buf).expect("reply after the heartbeat");
        assert!(matches!(frame, Frame::Reply { .. }));
    }

    #[test]
    fn a_false_mbap_header_resyncs_to_the_frame_behind_it() {
        // A request-shaped body passes protocol and length but fails the body
        // check; the parser must advance past it and find the real reply.
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&[
            0xde, 0xad, 0x00, 0x00, 0x00, 0x06, 0x01, 0x03, 0x00, 0x05, 0x80, 0x00, 0x0a,
        ]);
        buf.extend_from_slice(&tcp_frame(0x1234, &reply_pdu()));
        let frame = next_frame_tcp(&mut buf).expect("resync finds the reply");
        assert!(matches!(frame, Frame::Reply { .. }));
    }

    #[test]
    fn an_incomplete_heartbeat_waits_rather_than_consuming_the_stream() {
        // A logger counter frame split across segments: the parser must wait
        // for the rest of it, not resync byte by byte through what it has.
        let mut buf = BytesMut::from(&counter_frame()[..9]);
        assert!(next_frame_tcp(&mut buf).is_none(), "waiting for the rest");
        buf.extend_from_slice(&counter_frame()[9..]);
        buf.extend_from_slice(&tcp_frame(0x1234, &reply_pdu()));
        let frame = next_frame_tcp(&mut buf).expect("reply after the completed heartbeat");
        assert!(matches!(frame, Frame::Reply { .. }));
    }

    #[test]
    fn a_short_logger_frame_is_resynced_through() {
        // A short logger frame carries 0x0010 where MBAP has the protocol id, so
        // the parser rules the header out one byte at a time and still finds
        // the real reply behind it.
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&[0xa5, 0x17, 0x00, 0x10, 0x45, 0x03, 0x00, 0xef, 0x02]);
        buf.extend_from_slice(&tcp_frame(0x1234, &reply_pdu()));
        let frame = next_frame_tcp(&mut buf).expect("resync finds the reply");
        assert!(matches!(frame, Frame::Reply { .. }));
    }

    #[test]
    fn a_lying_length_field_is_resynced_to_the_reply_behind_it() {
        // A header claiming len 240 while the byte count says 10 can never
        // complete. The contradiction is visible at 9 bytes; without the early
        // check the parser waits out the claim and the read times out.
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&[0x00, 0x01, 0x00, 0x00, 0x00, 0xf0, 0x01, 0x03, 0x0a]);
        buf.extend_from_slice(&tcp_frame(0x1234, &reply_pdu()));
        let frame = next_frame_tcp(&mut buf).expect("resync finds the reply");
        assert!(matches!(frame, Frame::Reply { .. }));
    }

    #[test]
    fn a_false_logger_head_does_not_wipe_the_reply_behind_it() {
        // A logger-shaped head whose end byte is wrong was never a frame, and
        // is resynced one byte at a time. Skipping to the next `0xA5` instead
        // would consume the reply queued behind it.
        let mut bad = counter_frame();
        bad.pop();
        bad.push(0x16); // break the `0x15` end marker
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&bad);
        buf.extend_from_slice(&tcp_frame(0x1234, &reply_pdu()));
        let frame = next_frame_tcp(&mut buf).expect("resync finds the reply");
        assert!(matches!(frame, Frame::Reply { .. }));
        assert!(buf.is_empty(), "the reply behind the bad head survived");
    }

    #[test]
    fn a_false_logger_head_claiming_a_huge_length_does_not_stall() {
        // A logger-shaped head whose length field is absurd must be resynced, not
        // waited out: the read would otherwise time out against a frame that
        // can never complete.
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&[0xa5, 0xff, 0xff, 0x10, 0x47, 0x00]);
        buf.extend_from_slice(&tcp_frame(0x1234, &reply_pdu()));
        let frame = next_frame_tcp(&mut buf).expect("the reply parses");
        assert!(matches!(frame, Frame::Reply { .. }));
        assert!(buf.is_empty(), "the reply behind the false head survived");
    }

    #[test]
    fn two_modbus_tcp_frames_back_to_back_parse_as_two() {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&tcp_frame(0x1234, &reply_pdu()));
        buf.extend_from_slice(&tcp_frame(0x5678, &reply_pdu()));
        let first = next_frame_tcp(&mut buf).expect("first reply");
        let second = next_frame_tcp(&mut buf).expect("second reply");
        assert!(matches!(
            (first, second),
            (Frame::Reply { .. }, Frame::Reply { .. })
        ));
        assert!(buf.is_empty());
    }
}
