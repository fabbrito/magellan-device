//! Reassembly behaviour: incremental feeds, resync, and noise.
//!
//! One recv is never one frame, so every one of these is a shape the socket really produces.

use crate::wire::frame::{Frame, next_frame_tcp};
use crate::wire::modbus::registers;
use tokio_util::bytes::BytesMut;

use super::read_hex;

/// An MBAP frame carrying `pdu`, addressed to unit 1.
fn mbap(txn: u16, pdu: &[u8]) -> Vec<u8> {
    let mut frame = txn.to_be_bytes().to_vec();
    frame.extend_from_slice(&[0, 0]);
    // Saturating rather than unwrapped: a helper outside a #[test] fn does not get the lint's
    // test exemption, and a pdu this long would fail the assertion loudly anyway.
    let length = u16::try_from(pdu.len() + 1).unwrap_or(u16::MAX);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.push(1); // unit
    frame.extend_from_slice(pdu);
    frame
}

#[test]
fn byte_at_a_time_feed_assembles_exactly() {
    let bytes = read_hex("tcp-range-0580.hex").unwrap();
    let mut buf = BytesMut::new();
    let mut frame = None;
    for (i, &b) in bytes.iter().enumerate() {
        buf.extend_from_slice(&[b]);
        let got = next_frame_tcp(&mut buf);
        if i + 1 < bytes.len() {
            assert!(got.is_none(), "frame complete early at byte {i}");
        } else {
            frame = got;
        }
    }
    let Frame::Reply { rtu, .. } = frame.expect("complete frame") else {
        panic!("expected a reply");
    };
    assert_eq!(registers(&rtu).unwrap().len(), 10);
}

#[test]
fn a_false_start_resyncs_and_still_decodes() {
    let mut buf = BytesMut::new();
    // Garbage, including bytes that look like the head of the logger's own framing.
    buf.extend_from_slice(&[0x00, 0xa5, 0x01, 0x02, 0xa5, 0x00, 0x00]);
    buf.extend_from_slice(&read_hex("tcp-range-0580.hex").unwrap());
    let frame = next_frame_tcp(&mut buf).expect("resync finds the real frame");
    let Frame::Reply { rtu, .. } = frame else {
        panic!("expected a reply");
    };
    assert_eq!(registers(&rtu).unwrap().len(), 10);
}

#[test]
fn only_fc3_bodies_are_believed_as_read_replies() {
    // Two bodies identical but for the function code. MBAP carries no checksum, so the agreement
    // between function code, length and byte count is the whole of what rules a false header
    // out — every code it accepts is one more it can no longer reject.
    let mut buf = BytesMut::from(&mbap(0x1234, &[0x03, 0x02, 0x00, 0x83])[..]);
    assert!(matches!(
        next_frame_tcp(&mut buf),
        Some(Frame::Reply { .. })
    ));

    // Not a refusal — not a frame at all. The bytes are consumed looking for one.
    let mut buf = BytesMut::from(&mbap(0x1234, &[0x04, 0x02, 0x00, 0x83])[..]);
    assert!(next_frame_tcp(&mut buf).is_none());
}

#[test]
fn garbage_never_panics_or_hallucinates() {
    // Deterministic pseudo-random feed: reassembly must always terminate and return either a
    // frame or consumed input — never panic, never spin, never invent a reading.
    let mut buf = BytesMut::new();
    let mut state: u32 = 0x1234_5678;
    for _ in 0..10_000 {
        // LCG (Numerical Recipes constants); cheap deterministic noise.
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        buf.extend_from_slice(&[
            (state >> 24) as u8,
            (state >> 16) as u8,
            (state >> 8) as u8,
            state as u8,
        ]);
        let mut count = 0;
        while let Some(frame) = next_frame_tcp(&mut buf) {
            assert!(
                !matches!(frame, Frame::Reply { .. }),
                "noise decoded as a reply"
            );
            count += 1;
            assert!(count <= 4, "runaway frames from 4 bytes of input");
        }
    }
}
