//! Decoding a Modbus read reply into register values.
//!
//! Modbus TCP carries no RTU checksum. The MBAP length field and the reply's own byte count stand
//! in for one, and both are already checked by the time a body arrives here.

use crate::error::Error;

/// Decode an FC3/FC4 response body into register values.
///
/// Callers reach here only with sane bodies ([`crate::Frame::Reply`]); the checks
/// are defensive, not a substitute for classification.
///
/// # Errors
///
/// [`Error::ModbusException`] on an exception reply (fc | 0x80);
/// [`Error::Malformed`] on a body that is not a well-formed read reply.
pub fn registers(rtu: &[u8]) -> Result<Vec<u16>, Error> {
    let fc = *rtu.get(1).ok_or(Error::Malformed)?;
    let count = *rtu.get(2).ok_or(Error::Malformed)?;
    if fc & 0x80 != 0 {
        return Err(Error::ModbusException {
            fc: fc & 0x7F,
            code: count,
        });
    }
    let data = rtu.get(3..3 + usize::from(count)).ok_or(Error::Malformed)?;
    let mut out = Vec::with_capacity(data.len() / 2);
    for pair in data.as_chunks::<2>().0 {
        out.push(u16::from_be_bytes(*pair));
    }
    Ok(out)
}
