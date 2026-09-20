//! Layer 5 — the device runtime: config, clock, scheduling, buffer, upload, health.
//!
//! The buffer is what stands between a poor connection and a hole in the record. Bounded on
//! purpose: full, it drops the oldest batch and leaves a visible `seq` gap rather than dying — a
//! health signal, never something hidden.

use contract::Batch;

/// A bounded, at-least-once queue of batches awaiting upload.
///
/// A batch leaves on `2xx` or a permanent `4xx`; `429`, `503`, `5xx`, a rejected credential
/// (`401`/`403`) and no answer keep it — a rotated token is not worth the readings it would drop.
/// Duplicates are absorbed cloud-side.
pub trait Buffer {
    /// Batches still queued, oldest first — the heartbeat's `buffer_depth`.
    fn depth(&self) -> u32;

    /// Append at the tail with the next `seq`. When full, the oldest batch is dropped; the `seq`
    /// gap is the point.
    fn push(&mut self, batch: Batch);

    /// The oldest queued batch, without removing it.
    fn peek(&self) -> Option<&Batch>;

    /// Drop the oldest batch on `2xx` or a permanent `4xx` — `401`/`403` excepted, which keep it.
    fn pop(&mut self);
}
