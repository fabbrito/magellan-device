//! Modbus reads, as every driver that speaks Modbus sends and decodes them.
//!
//! Read-only by construction: [`ReadFunction`] has no write code, so no driver built on this crate
//! can ask a source to change. How a reply is cut out of the stream stays with the driver — a
//! well-behaved server needs only the MBAP length; a noisy one, like the Sofar logger, needs its
//! own resync.

mod error;

pub use crate::error::ModbusError;

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

/// Decode a read reply into register values.
///
/// `body` is what MBAP carries after its length field: the unit id, then the PDU. Modbus TCP has
/// no CRC; the MBAP length and the reply's own byte count stand in for one, and the caller checks
/// the first before a body reaches here.
///
/// # Errors
///
/// [`ModbusError::Exception`] on an exception reply (function | 0x80);
/// [`ModbusError::Malformed`] on a body that is not a well-formed read reply.
pub fn registers(body: &[u8]) -> Result<Vec<u16>, ModbusError> {
    let function = *body.get(1).ok_or(ModbusError::Malformed)?;
    let count = *body.get(2).ok_or(ModbusError::Malformed)?;
    if function & 0x80 != 0 {
        return Err(ModbusError::Exception {
            function: function & 0x7F,
            code: count,
        });
    }
    let data = body
        .get(3..3 + usize::from(count))
        .ok_or(ModbusError::Malformed)?;
    let mut out = Vec::with_capacity(data.len() / 2);
    for pair in data.as_chunks::<2>().0 {
        out.push(u16::from_be_bytes(*pair));
    }
    Ok(out)
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
    fn a_reply_decodes_big_endian_in_order() {
        let body = [0x01, 0x04, 0x04, 0x00, 0x2a, 0xff, 0xb5];
        assert_eq!(registers(&body).unwrap(), [42, 0xffb5]);
    }

    #[test]
    fn an_exception_names_its_function_and_code() {
        let body = [0x01, 0x84, 0x02];
        assert!(matches!(
            registers(&body),
            Err(ModbusError::Exception {
                function: 4,
                code: 2
            })
        ));
    }

    #[test]
    fn a_byte_count_past_the_body_is_malformed() {
        let body = [0x01, 0x04, 0x04, 0x00, 0x2a];
        assert!(matches!(registers(&body), Err(ModbusError::Malformed)));
    }

    #[test]
    fn a_body_too_short_for_a_function_is_malformed() {
        assert!(matches!(registers(&[0x01]), Err(ModbusError::Malformed)));
    }
}
