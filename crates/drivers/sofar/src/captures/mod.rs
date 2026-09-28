//! The driver against bytes captured off a real logger: the codec, reassembly, and the shipped
//! profile. Inside the crate so they test its internals without making them its interface.

mod codec;
mod golden;
mod reassembly;

use std::fs;
use std::path::PathBuf;

pub(crate) type BoxError = Box<dyn std::error::Error>;

pub(crate) fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/captures/fixtures")
        .join(name)
}

pub(crate) fn read_hex(name: &str) -> Result<Vec<u8>, BoxError> {
    let text = fs::read_to_string(fixture(name))?;
    text.split_whitespace()
        .map(|tok| u8::from_str_radix(tok, 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}
