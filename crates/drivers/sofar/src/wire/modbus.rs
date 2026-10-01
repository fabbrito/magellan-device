//! Decoding a Modbus read reply into register values.
//!
//! Modbus TCP carries no RTU checksum. The MBAP length field and the reply's own byte count stand
//! in for one, and both are already checked by the time a body arrives here.

use crate::wire::WireError;

/// Decode an FC3/FC4 response body into register values.
///
/// Callers reach here only with sane bodies ([`crate::Frame::Reply`]); the checks
/// are defensive, not a substitute for classification.
///
/// # Errors
///
/// [`WireError::ModbusException`] on an exception reply (fc | 0x80);
/// [`WireError::Malformed`] on a body that is not a well-formed read reply.
pub fn registers(rtu: &[u8]) -> Result<Vec<u16>, WireError> {
    let fc = *rtu.get(1).ok_or(WireError::Malformed)?;
    let count = *rtu.get(2).ok_or(WireError::Malformed)?;
    if fc & 0x80 != 0 {
        return Err(WireError::ModbusException {
            fc: fc & 0x7F,
            code: count,
        });
    }
    let data = rtu
        .get(3..3 + usize::from(count))
        .ok_or(WireError::Malformed)?;
    let mut out = Vec::with_capacity(data.len() / 2);
    for pair in data.as_chunks::<2>().0 {
        out.push(u16::from_be_bytes(*pair));
    }
    Ok(out)
}
