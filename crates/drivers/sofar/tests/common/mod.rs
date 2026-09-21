//! Fixture loading, shared by the test crates.

use std::fs;
use std::path::PathBuf;

pub type BoxError = Box<dyn std::error::Error>;

pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

pub fn read_hex(name: &str) -> Result<Vec<u8>, BoxError> {
    let text = fs::read_to_string(fixture(name))?;
    text.split_whitespace()
        .map(|tok| u8::from_str_radix(tok, 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}
