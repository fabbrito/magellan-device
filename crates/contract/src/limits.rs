//! The contract's bounds, mirrored.
//!
//! The cloud authors them; the copy is here because a bound written in a comment enforces nothing,
//! and the device's native integers reach past what the cloud accepts in both directions that
//! matter.
//!
//! Transcribed from the published document. The relationships between them are asserted below,
//! so a transcription that breaks one breaks the build rather than the wire.

/// One to this many sources in a manifest.
pub const SOURCES_MAX: usize = 8;

/// One to this many metrics in a source, and values in a reading.
pub const METRICS_PER_SOURCE_MAX: usize = 128;

/// One to this many readings in a batch. The cloud's ceiling on one commit, not the device's
/// buffer bound: a device sends what it holds, up to this.
pub const READINGS_PER_BATCH_MAX: usize = 512;

/// Bytes in a source id or metric key. ASCII, so bytes are characters.
pub const KEY_LENGTH_MAX: usize = 64;

/// Bytes in a measured metric's unit.
pub const UNIT_LENGTH_MAX: usize = 16;

/// A metric's decimal exponent: the value is `value × 10^exponent`.
pub const EXPONENT_MIN: i8 = -12;

/// A metric's decimal exponent: the value is `value × 10^exponent`.
pub const EXPONENT_MAX: i8 = 12;

/// Where the cloud's parser stops being exact, not where a value is expected to reach.
///
/// A double's mantissa is 53 bits. The device refuses past it rather than sending an integer the
/// other side would round. Narrower than `i64` and far wider than `i32`, so no Rust type is the
/// bound.
pub const METRIC_VALUE_MAX: i64 = (1 << 53) - 1;

/// Symmetric with [`METRIC_VALUE_MAX`]: exactness has no sign.
pub const METRIC_VALUE_MIN: i64 = -METRIC_VALUE_MAX;

/// Digits in a state's label code.
pub const STATE_CODE_DIGITS_MAX: usize = 9;

/// Labels a state metric may carry.
pub const STATE_LABELS_MAX: usize = 32;

/// Bytes in one state label.
pub const STATE_LABEL_LENGTH_MAX: usize = 32;

/// Digits in a `seq`: the counter is a `u64`, so this is how wide `u64::MAX` is written.
pub const SEQ_DIGITS_MAX: usize = u64::MAX.ilog10() as usize + 1;

/// Lowercase hex digits in a manifest hash: SHA-256, four bits to the digit.
pub const MANIFEST_HASH_HEX_LENGTH: usize = 256 / 4;

/// Milliseconds since the Unix epoch, UTC. The year 3000, so a clock that never synced is caught
/// rather than archived.
pub const TIMESTAMP_MS_MAX: u64 = 32_503_680_000_000;

/// Seconds since boot: ten Julian years, past which the counter is not to be believed.
pub const UPTIME_SECONDS_MAX: u64 = 10 * 31_557_600;

/// Batches a heartbeat may claim to hold.
pub const BUFFER_DEPTH_MAX: u32 = 1_000_000;

/// Percent, so full is a hundred.
pub const BATTERY_PERCENT_MAX: u8 = 100;

/// Signal strength, reported as a percentage of usable.
pub const SIGNAL_PERCENT_MAX: u8 = 100;

/// Bytes in a reported firmware version.
pub const FIRMWARE_VERSION_LENGTH_MAX: usize = 32;

/// Lowercase hex digits in a boot id.
pub const BOOT_ID_LENGTH_MIN: usize = 8;

/// Lowercase hex digits in a boot id.
pub const BOOT_ID_LENGTH_MAX: usize = 32;

/// Bytes in a manifest as sent. The counts above do not imply it: a label is bounded in bytes
/// before JSON escapes it, and an escape is up to six.
pub const MANIFEST_BYTES_MAX: usize = 1_999_000;

// The bounds above that have a derivation are written as one. What is left is the cloud's policy,
// transcribed, so the relationships between those are asserted instead: a pair inverted by a bad
// transcription stops the build rather than the first upload.
const _: () = assert!(EXPONENT_MIN < 0 && EXPONENT_MAX > 0);
const _: () = assert!(BOOT_ID_LENGTH_MIN <= BOOT_ID_LENGTH_MAX);
const _: () = assert!(METRIC_VALUE_MAX > i32::MAX as i64);
const _: () = assert!(TIMESTAMP_MS_MAX < METRIC_VALUE_MAX.cast_unsigned());

#[cfg(test)]
mod tests {
    use super::*;

    // The numbers as the cloud authors them, written out. Every other test builds its case from
    // the constants above, so it moves with them and proves only that the code agrees with itself;
    // this one is where a transcription that drifted from the cloud is caught. Change it only
    // against the published document.
    #[test]
    fn the_limits_are_the_clouds() {
        assert_eq!(SOURCES_MAX, 8);
        assert_eq!(METRICS_PER_SOURCE_MAX, 128);
        assert_eq!(READINGS_PER_BATCH_MAX, 512);
        assert_eq!(KEY_LENGTH_MAX, 64);
        assert_eq!(UNIT_LENGTH_MAX, 16);
        assert_eq!(EXPONENT_MIN, -12);
        assert_eq!(EXPONENT_MAX, 12);
        assert_eq!(METRIC_VALUE_MIN, -9_007_199_254_740_991);
        assert_eq!(METRIC_VALUE_MAX, 9_007_199_254_740_991);
        assert_eq!(STATE_CODE_DIGITS_MAX, 9);
        assert_eq!(STATE_LABELS_MAX, 32);
        assert_eq!(STATE_LABEL_LENGTH_MAX, 32);
        assert_eq!(SEQ_DIGITS_MAX, 20);
        assert_eq!(MANIFEST_HASH_HEX_LENGTH, 64);
        assert_eq!(TIMESTAMP_MS_MAX, 32_503_680_000_000);
        assert_eq!(UPTIME_SECONDS_MAX, 315_576_000);
        assert_eq!(BUFFER_DEPTH_MAX, 1_000_000);
        assert_eq!(BATTERY_PERCENT_MAX, 100);
        assert_eq!(SIGNAL_PERCENT_MAX, 100);
        assert_eq!(FIRMWARE_VERSION_LENGTH_MAX, 32);
        assert_eq!(BOOT_ID_LENGTH_MIN, 8);
        assert_eq!(BOOT_ID_LENGTH_MAX, 32);
        assert_eq!(MANIFEST_BYTES_MAX, 1_999_000);
    }
}
