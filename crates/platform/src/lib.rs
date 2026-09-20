//! Layer 7 — the platform seam. The runtime never names an OS; a platform supplies the clock, the
//! sleep and the flash the buffer spills to. Two are aimed at — Linux (Pi) and `esp-idf-svc`
//! (ESP32) — so the runtime is written once against this shape.

/// Wall-clock and monotonic time, in the units the contract and the heartbeat use.
pub trait Clock {
    /// Milliseconds since the Unix epoch, UTC — the `Reading::ts` the runtime stamps.
    fn now_ms(&self) -> u64;

    /// Seconds since boot. The heartbeat's `boot_id` is what makes a reset explainable, not uptime.
    fn uptime_seconds(&self) -> u64;
}
