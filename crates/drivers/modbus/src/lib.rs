//! Modbus reads, as every driver that speaks Modbus sends and decodes them.
//!
//! Read-only by construction: [`ReadFunction`] has no write code, so no driver built on this crate
//! can ask a source to change. How a reply is cut out of the stream stays with the driver — a
//! well-behaved server needs only the MBAP length; a noisy one, like the Sofar logger, needs its
//! own resync.
//!
//! Specs, from <https://www.modbus.org/modbus-specifications>:
//!
//! - MODBUS Application Protocol Specification — the PDU: the reads and the exceptions.
//!   <https://www.modbus.org/file/secure/modbusprotocolspecification.pdf>
//! - MODBUS Messaging on TCP/IP Implementation Guide — the MBAP header and the client's handling
//!   of a reply. <https://www.modbus.org/file/secure/messagingimplementationguide.pdf>

mod error;

pub use crate::error::{ExceptionCode, ModbusError};

/// Most registers one read may ask for: the reply counts its data in one byte.
pub const QUANTITY_MAX: u16 = 125;
/// MBAP bytes before the unit id: transaction id, protocol id, length.
pub const MBAP_PREFIX_SIZE: usize = 6;
/// Modbus TCP requires protocol id `00 00`; anything else is not an MBAP frame.
pub const MBAP_PROTOCOL_ID: u16 = 0;
/// Largest MBAP length field: the unit id plus a 253-byte PDU.
pub const MBAP_LENGTH_MAX: u16 = 254;
/// A read request on the wire: MBAP prefix, unit id, then a five-byte PDU.
pub const READ_REQUEST_SIZE: usize = 12;

// The fullest reply fits one frame: unit id, function, byte count, then the registers.
const _: () = assert!(3 + 2 * QUANTITY_MAX <= MBAP_LENGTH_MAX);

/// The function codes a read may carry. Write codes are absent on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ReadFunction {
    /// FC3, read holding registers.
    Holding = 0x03,
    /// FC4, read input registers.
    Input = 0x04,
}

impl ReadFunction {
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }
}

/// One read, framed as Modbus TCP.
///
/// The quantity's bound, [`QUANTITY_MAX`], is the caller's to enforce where it plans its reads; a
/// request past it is encoded faithfully and refused by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRequest {
    /// The MBAP transaction id a reply must echo.
    pub transaction: u16,
    pub unit: u8,
    pub function: ReadFunction,
    pub address: u16,
    pub quantity: u16,
}

impl ReadRequest {
    /// The request's bytes: transaction, protocol, length, unit, function, address, quantity.
    #[must_use]
    pub fn encode(&self) -> [u8; READ_REQUEST_SIZE] {
        let [transaction_high, transaction_low] = self.transaction.to_be_bytes();
        let [protocol_high, protocol_low] = MBAP_PROTOCOL_ID.to_be_bytes();
        let [address_high, address_low] = self.address.to_be_bytes();
        let [quantity_high, quantity_low] = self.quantity.to_be_bytes();
        // The length field counts what follows it: the unit id and the five-byte PDU.
        let [length_high, length_low] =
            ((READ_REQUEST_SIZE - MBAP_PREFIX_SIZE) as u16).to_be_bytes();
        [
            transaction_high,
            transaction_low,
            protocol_high,
            protocol_low,
            length_high,
            length_low,
            self.unit,
            self.function.code(),
            address_high,
            address_low,
            quantity_high,
            quantity_low,
        ]
    }
}

/// What an MBAP prefix says about the frame behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MbapPrefix {
    pub transaction: u16,
    /// Bytes after the prefix: the unit id, then the PDU. Within `1..=MBAP_LENGTH_MAX`.
    pub body_size: u16,
}

/// Read an MBAP prefix off a well-behaved server, where the length field is trusted to frame.
///
/// # Errors
///
/// [`ModbusError::Malformed`] when the protocol id is not Modbus or the length cannot be a frame:
/// a stream that framing cannot follow, so the connection is over.
pub fn mbap_prefix(prefix: [u8; MBAP_PREFIX_SIZE]) -> Result<MbapPrefix, ModbusError> {
    let [
        transaction_high,
        transaction_low,
        protocol_high,
        protocol_low,
        length_high,
        length_low,
    ] = prefix;
    if u16::from_be_bytes([protocol_high, protocol_low]) != MBAP_PROTOCOL_ID {
        return Err(ModbusError::Malformed);
    }
    let body_size = u16::from_be_bytes([length_high, length_low]);
    if !(1..=MBAP_LENGTH_MAX).contains(&body_size) {
        return Err(ModbusError::Malformed);
    }
    Ok(MbapPrefix {
        transaction: u16::from_be_bytes([transaction_high, transaction_low]),
        body_size,
    })
}

/// Decode the reply to a read of `quantity` registers by `function` into their values.
///
/// `body` is what MBAP carries after its length field: the unit id, then the PDU. Modbus TCP has
/// no CRC; the MBAP length and the reply's own byte count stand in for one, so both are held to
/// exactly what the read asked: the function echoed, two bytes per register.
///
/// # Errors
///
/// [`ModbusError::Exception`] on an exception reply to `function` (function | 0x80);
/// [`ModbusError::Malformed`] on anything that is not the reply to this read.
pub fn registers(
    body: &[u8],
    function: ReadFunction,
    quantity: u16,
) -> Result<Vec<u16>, ModbusError> {
    let &[_unit, echoed, count_or_code, ref data @ ..] = body else {
        return Err(ModbusError::Malformed);
    };
    if echoed & 0x80 != 0 {
        if echoed & 0x7F != function.code() || !data.is_empty() {
            return Err(ModbusError::Malformed);
        }
        return Err(ModbusError::Exception {
            function: function.code(),
            code: ExceptionCode(count_or_code),
        });
    }
    if echoed != function.code() {
        return Err(ModbusError::Malformed);
    }
    if usize::from(count_or_code) != 2 * usize::from(quantity) {
        return Err(ModbusError::Malformed);
    }
    if data.len() != usize::from(count_or_code) {
        return Err(ModbusError::Malformed);
    }
    let mut out = Vec::with_capacity(usize::from(quantity));
    for pair in data.as_chunks::<2>().0 {
        out.push(u16::from_be_bytes(*pair));
    }
    Ok(out)
}

/// The exception code a reply carries, if it is an exception reply.
///
/// For a body already judged a refusal, where only the reason is wanted; [`registers`] is how a
/// reply is read.
#[must_use]
pub fn exception(body: &[u8]) -> Option<ExceptionCode> {
    let &[_unit, function, code] = body else {
        return None;
    };
    (function & 0x80 != 0).then_some(ExceptionCode(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_request_is_exactly_mbap_unit_pdu() {
        let request = ReadRequest {
            transaction: 0x1234,
            unit: 1,
            function: ReadFunction::Input,
            address: 0x0580,
            quantity: 10,
        };
        assert_eq!(
            request.encode(),
            [
                0x12, 0x34, 0x00, 0x00, 0x00, 0x06, 0x01, 0x04, 0x05, 0x80, 0x00, 0x0a
            ]
        );
    }

    #[test]
    fn a_prefix_gives_its_transaction_and_body_sizegth() {
        let prefix = mbap_prefix([0x12, 0x34, 0x00, 0x00, 0x00, 0x11]).unwrap();
        assert_eq!(
            prefix,
            MbapPrefix {
                transaction: 0x1234,
                body_size: 17
            }
        );
    }

    #[test]
    fn a_prefix_of_another_protocol_is_malformed() {
        let prefix = mbap_prefix([0x12, 0x34, 0x00, 0x01, 0x00, 0x11]);
        assert!(matches!(prefix, Err(ModbusError::Malformed)));
    }

    #[test]
    fn a_prefix_length_no_frame_can_have_is_malformed() {
        for length in [0, MBAP_LENGTH_MAX + 1] {
            let [high, low] = length.to_be_bytes();
            let prefix = mbap_prefix([0x12, 0x34, 0x00, 0x00, high, low]);
            assert!(matches!(prefix, Err(ModbusError::Malformed)), "{length}");
        }
    }

    #[test]
    fn a_reply_decodes_big_endian_in_order() {
        let body = [0x01, 0x04, 0x04, 0x00, 0x2a, 0xff, 0xb5];
        assert_eq!(
            registers(&body, ReadFunction::Input, 2).unwrap(),
            [42, 0xffb5]
        );
    }

    #[test]
    fn the_spec_example_reads_registers_108_to_110() {
        // The application protocol's own example: 02 2B, 00 00, 00 64 are 555, 0 and 100.
        let body = [0x01, 0x03, 0x06, 0x02, 0x2b, 0x00, 0x00, 0x00, 0x64];
        assert_eq!(
            registers(&body, ReadFunction::Holding, 3).unwrap(),
            [555, 0, 100]
        );
    }

    #[test]
    fn an_exception_names_its_code() {
        let body = [0x01, 0x84, 0x02];
        let Err(ModbusError::Exception { function, code }) =
            registers(&body, ReadFunction::Input, 7)
        else {
            panic!("expected an exception");
        };
        assert_eq!((function, code), (4, ExceptionCode::ILLEGAL_DATA_ADDRESS));
        assert!(code.is_request_fault());
        assert_eq!(code.to_string(), "illegal data address (0x02)");
    }

    #[test]
    fn an_exception_body_gives_its_code_and_a_reply_none() {
        assert_eq!(
            exception(&[0x01, 0x83, 0x02]),
            Some(ExceptionCode::ILLEGAL_DATA_ADDRESS)
        );
        assert_eq!(exception(&[0x01, 0x03, 0x02, 0x00, 0x2a]), None);
    }

    #[test]
    fn a_server_fault_is_not_a_request_fault() {
        assert!(!ExceptionCode::SERVER_DEVICE_FAILURE.is_request_fault());
        assert_eq!(ExceptionCode(0x07).to_string(), "0x07");
    }

    #[test]
    fn what_is_not_the_reply_to_this_read_is_malformed() {
        // (case, quantity asked, body)
        let cases: [(&str, u16, &[u8]); 7] = [
            ("too short for a function", 1, &[0x01]),
            (
                "another function echoed",
                1,
                &[0x01, 0x03, 0x02, 0x00, 0x2a],
            ),
            ("a write echoed", 1, &[0x01, 0x06, 0x02, 0x00, 0x2a]),
            (
                "an odd byte count",
                1,
                &[0x01, 0x04, 0x03, 0x00, 0x2a, 0x00],
            ),
            (
                "fewer registers than asked",
                2,
                &[0x01, 0x04, 0x02, 0x00, 0x2a],
            ),
            ("a count past the body", 2, &[0x01, 0x04, 0x04, 0x00, 0x2a]),
            ("an exception to another function", 1, &[0x01, 0x83, 0x02]),
        ];
        for (case, quantity, body) in cases {
            assert!(
                matches!(
                    registers(body, ReadFunction::Input, quantity),
                    Err(ModbusError::Malformed)
                ),
                "{case}"
            );
        }
    }

    #[test]
    fn bytes_behind_the_count_are_malformed() {
        let body = [0x01, 0x04, 0x02, 0x00, 0x2a, 0xff];
        assert!(matches!(
            registers(&body, ReadFunction::Input, 1),
            Err(ModbusError::Malformed)
        ));
    }
}
