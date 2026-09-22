//! Layer 7 — the platform seam. The runtime never names an OS; a platform supplies the clock, the
//! sleep and the flash the buffer spills to. One platform is built — Linux on 32-bit ARM — and the
//! seam stays anyway, because the runtime is written against a shape rather than against an OS.

use std::fs::File;
use std::io::{self, Read};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use contract::limits::{BOOT_ID_LENGTH_MAX, BOOT_ID_LENGTH_MIN};

/// Wall-clock and monotonic time, in the units the contract and the heartbeat use.
pub trait Clock {
    /// Milliseconds since the Unix epoch, UTC — the `Reading::ts` the runtime stamps.
    fn now_ms(&self) -> u64;

    /// Seconds since boot. The heartbeat's `boot_id` is what makes a reset explainable, not uptime.
    fn uptime_seconds(&self) -> u64;
}

/// The Linux clock: wall time from the OS, uptime from a monotonic instant taken at construction.
///
/// Uptime is therefore since this process started, not since the board powered on. For a device
/// whose binary starts at boot they are the same number, and where they differ the process restart
/// is the event worth seeing — it comes paired with a fresh `boot_id`.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    started: Instant,
}

impl SystemClock {
    /// Start the uptime count. Call once, where the program starts.
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        // A Pi carries no clock of its own until NTP steps it, so this reads 1970 for the first
        // seconds of a boot. Reported as it is: a timestamp the runtime can see is wrong beats one
        // the platform quietly invented.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            })
    }

    fn uptime_seconds(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}

/// Bytes of entropy behind a boot id: sixteen hex digits, inside the contract's 8..=32.
const BOOT_ID_BYTES: usize = 8;

/// A fresh boot id, drawn from the operating system's random source.
///
/// Half of what identifies a batch (ADR 8), so it has to differ between two runs of the same
/// device or the second run's readings are deduplicated away as replays of the first. Entropy,
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

/// A seed for decorrelating one device from the rest of the fleet.
///
/// The kernel's bytes, or the clock where they cannot be read. Unlike a boot id a fallback is
/// sound: a shared seed costs a spread, never a reading.
#[must_use]
pub fn random_seed() -> u64 {
    let mut bytes = [0_u8; 8];
    let drawn = File::open("/dev/urandom").and_then(|mut source| source.read_exact(&mut bytes));
    if drawn.is_ok() {
        return u64::from_le_bytes(bytes);
    }
    SystemClock::new().now_ms()
}

const _: () = assert!(BOOT_ID_BYTES * 2 >= BOOT_ID_LENGTH_MIN);
const _: () = assert!(BOOT_ID_BYTES * 2 <= BOOT_ID_LENGTH_MAX);

#[cfg(test)]
mod tests {
    use super::*;

    /// 2020-01-01T00:00:00Z in milliseconds. A real clock is past it; the same instant expressed
    /// in seconds is not, so this one bound catches both a dead clock and a units slip.
    const A_DATE_ALREADY_PAST: u64 = 1_577_836_800_000;

    #[test]
    fn the_wall_clock_reads_milliseconds_past_a_date_already_gone() {
        assert!(SystemClock::new().now_ms() > A_DATE_ALREADY_PAST);
    }

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
            heartbeat: None,
        };
        assert_eq!(batch.validate(), Ok(()));
    }

    #[test]
    fn two_boots_do_not_share_an_id() {
        // The whole of what ADR 8 rests on. A repeat here means the second boot's readings are
        // deduplicated away as replays of the first, silently.
        let ids: std::collections::BTreeSet<String> =
            (0..64).map(|_| boot_id().expect("readable")).collect();
        assert_eq!(ids.len(), 64, "a boot id repeated");
    }

    #[test]
    fn uptime_starts_at_zero_and_never_goes_back() {
        let clock = SystemClock::new();
        assert_eq!(clock.uptime_seconds(), 0);
        let first = clock.uptime_seconds();
        let second = clock.uptime_seconds();
        assert!(second >= first);
    }
}
