//! The per-boot identity half of a batch's key rests on (ADR 8).

use std::fs::File;
use std::io::{self, Read};

use contract::limits::{BOOT_ID_LENGTH_MAX, BOOT_ID_LENGTH_MIN};

/// Bytes of entropy behind a boot id: sixteen hex digits, inside the contract's 8..=32.
const BOOT_ID_BYTES: usize = 8;

/// A fresh boot id, drawn from the operating system's random source.
///
/// Half of what identifies a batch (ADR 8), so it has to differ between two runs of the same
/// device or the second run's gaps hide behind the first's sequence. Entropy,
/// not a counter: the alternative was a number written to flash, which is the thing that decision
/// removed.
///
/// # Errors
///
/// If the random source cannot be read. There is no fallback on purpose — a boot id the device
/// invented from its clock would collide exactly when the clock has not been set, which is every
/// cold boot without a network.
pub fn boot_id() -> io::Result<String> {
    let mut bytes = [0_u8; BOOT_ID_BYTES];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex::encode(bytes))
}

const _: () = assert!(BOOT_ID_BYTES * 2 >= BOOT_ID_LENGTH_MIN);
const _: () = assert!(BOOT_ID_BYTES * 2 <= BOOT_ID_LENGTH_MAX);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boot_id_is_one_the_contract_accepts() {
        // Checked against the contract rather than against a length: this value goes on the wire
        // as half of what identifies a batch, so the cloud's rule is the one that matters.
        let batch = contract::Batch {
            manifest_hash: "0".repeat(64),
            boot_id: boot_id().expect("the random source is readable"),
            seq: "1".to_owned(),
            readings: vec![contract::Reading {
                source: "source_1".to_owned(),
                ts: 1_758_326_400_000,
                values: std::collections::BTreeMap::from([("power_w".to_owned(), 1_i64)]),
            }],
        };
        assert_eq!(batch.validate(), Ok(()));
    }

    #[test]
    fn two_boots_do_not_share_an_id() {
        // The whole of what ADR 8 rests on. A repeat here aliases two boots in gap detection, and
        // a loss in one hides behind the other's sequence.
        let ids: std::collections::BTreeSet<String> =
            (0..64).map(|_| boot_id().expect("readable")).collect();
        assert_eq!(ids.len(), 64, "a boot id repeated");
    }
}
