//! The bounded buffer of batches awaiting upload: refused, stamped, queued and released under one
//! lock.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::{Mutex, MutexGuard, PoisonError};

use contract::{Batch, Manifest, Reading, Refusal};

/// A bounded, at-least-once queue of batches, oldest first, held in RAM and shared by the poll
/// and the drain.
///
/// Full, it drops the oldest batch rather than growing into the memory the rest of the device
/// needs. The dropped batch already carries its `seq`, so what reaches the cloud has a visible gap
/// where it was — a health signal, never something hidden (ADR 4).
///
/// The boot id is drawn once and the counter starts at zero, which together identify a batch
/// (ADR 8). Stamping and queueing share the lock, so `seq` order is queue order.
///
/// A reading the contract would refuse never reaches a batch (ADR 3): the cloud's `4xx` is not how
/// the device finds out.
#[derive(Debug)]
pub struct Buffer {
    manifest: Manifest,
    manifest_hash: String,
    boot_id: String,
    capacity: NonZeroUsize,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    batches: VecDeque<Batch>,
    seq: u64,
    dropped: u64,
}

/// What one [`Buffer::enqueue`] did — the journal's to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enqueued {
    /// Readings refused, by source. Dropped before stamping, so no `seq` gap shows them: this is
    /// the only account of the loss.
    pub refused: Vec<(String, Refusal)>,
    /// The batch queued, or `None` when every reading was refused and no `seq` was spent.
    pub queued: Option<Queued>,
}

/// The batch one [`Buffer::enqueue`] queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    /// The `seq` stamped on it.
    pub seq: String,
    /// Batches queued once it was.
    pub depth: u32,
    /// The `seq` pushed out the front to make room. It never reaches the cloud: the gap.
    pub displaced: Option<String>,
    /// Batches displaced over this buffer's life.
    pub dropped: u64,
}

impl Buffer {
    /// A buffer of at most `capacity` batches of readings `manifest` declares, stamped with the
    /// hash it is declared under, in the boot `boot_id` names.
    ///
    /// Non-zero by the type: a buffer that can hold nothing drops every reading the moment it is
    /// made, and would look like a working device doing it.
    #[must_use]
    pub fn new(
        capacity: NonZeroUsize,
        manifest: Manifest,
        manifest_hash: String,
        boot_id: String,
    ) -> Self {
        Self {
            manifest,
            manifest_hash,
            boot_id,
            capacity,
            state: Mutex::new(State {
                batches: VecDeque::with_capacity(capacity.get()),
                seq: 0,
                dropped: 0,
            }),
        }
    }

    /// Refuse what the contract would, stamp the rest as the next batch and append it, dropping the
    /// oldest when full.
    ///
    /// The `seq` is spent whether or not the batch is ever delivered — a number spent on a batch later dropped is exactly the gap
    /// that shows the loss.
    ///
    /// # Panics
    ///
    /// When the batch the buffer stamped breaks the contract. The readings in it have passed, so
    /// what broke is the envelope the runtime built itself: a programmer error, not an operating one.
    pub fn enqueue(&self, readings: Vec<Reading>) -> Enqueued {
        let mut refused = Vec::new();
        let readings: Vec<Reading> = readings
            .into_iter()
            .filter(|reading| {
                let checked = reading
                    .validate()
                    .and_then(|()| reading.check_against(&self.manifest));
                checked
                    .map_err(|why| refused.push((reading.source.clone(), why)))
                    .is_ok()
            })
            .collect();
        if readings.is_empty() {
            return Enqueued {
                refused,
                queued: None,
            };
        }

        let mut state = self.state();
        // One more, unless full and the oldest makes room.
        let after = state
            .batches
            .len()
            .saturating_add(1)
            .min(self.capacity.get());
        let depth = u32::try_from(after).unwrap_or(u32::MAX);
        let seq = state.seq.to_string();
        let batch = Batch {
            manifest_hash: self.manifest_hash.clone(),
            boot_id: self.boot_id.clone(),
            seq: seq.clone(),
            readings,
        };
        assert_eq!(
            batch.validate(),
            Ok(()),
            "the runtime stamped a malformed batch"
        );
        assert_eq!(
            batch.check_against(&self.manifest),
            Ok(()),
            "a batch names what the manifest does not declare"
        );
        // `u64` outlasts any device polling every few minutes. Saturating rather than wrapping so
        // the impossible case repeats one number instead of replaying the whole range as a
        // sequence gap detection cannot read.
        state.seq = state.seq.saturating_add(1);
        let displaced = if state.batches.len() >= self.capacity.get() {
            state.dropped = state.dropped.saturating_add(1);
            state.batches.pop_front().map(|dropped| dropped.seq)
        } else {
            None
        };
        state.batches.push_back(batch);
        Enqueued {
            refused,
            queued: Some(Queued {
                seq,
                depth,
                displaced,
                dropped: state.dropped,
            }),
        }
    }

    /// A copy of the oldest batch, so no lock is held across the send.
    #[must_use]
    pub fn front(&self) -> Option<Batch> {
        self.state().batches.front().cloned()
    }

    /// Drop `sent` if it is still the oldest batch. Returns whether it was.
    ///
    /// While a request is in flight the poll may overflow the buffer and push `sent` out, putting
    /// another at the front. Popping blindly would drop a batch that was never sent — a reading
    /// lost with no `seq` gap to show for it.
    pub fn release(&self, sent: &Batch) -> bool {
        let mut state = self.state();
        let still_ours = state
            .batches
            .front()
            .is_some_and(|front| front.boot_id == sent.boot_id && front.seq == sent.seq);
        if still_ours {
            state.batches.pop_front();
        }
        still_ours
    }

    /// Batches still queued.
    #[must_use]
    pub fn depth(&self) -> u32 {
        depth_of(&self.state().batches)
    }

    /// Every operation leaves the queue whole, so a lock poisoned by a panic elsewhere still
    /// guards a usable queue. Refusing it would stop the drain for good and drop every sweep
    /// silently.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Saturating: the contract bounds what it will accept, and a depth past `u32` means the bound was
/// never applied. Reporting the ceiling beats wrapping to nothing.
fn depth_of(batches: &VecDeque<Batch>) -> u32 {
    u32::try_from(batches.len()).unwrap_or(u32::MAX)
}

/// One source, `inverter`, declaring `power_w` — what the tests beside the buffer fill it with.
#[cfg(test)]
impl Buffer {
    pub(crate) fn fixture(capacity: usize) -> Self {
        let manifest = Manifest {
            tz: "UTC".to_owned(),
            sources: vec![contract::Source {
                id: "inverter".to_owned(),
                metrics: vec![contract::Metric::Gauge {
                    key: "power_w".to_owned(),
                    unit: Some("W".to_owned()),
                    exponent: -2,
                }],
            }],
        };
        Self::new(
            NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN),
            manifest,
            "0".repeat(64),
            "0123456789abcdef".to_owned(),
        )
    }

    /// Queue `batches` sweeps of one declared reading, stamped from the next `seq` on.
    pub(crate) fn fill(&self, batches: u64) {
        for _ in 0..batches {
            let reading = Reading {
                source: "inverter".to_owned(),
                ts: 1_758_326_400_000,
                values: std::collections::BTreeMap::from([("power_w".to_owned(), 27_034_i64)]),
            };
            self.enqueue(vec![reading]);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn buffer(capacity: usize) -> Buffer {
        Buffer::fixture(capacity)
    }

    fn reading(source: &str, key: &str, value: i64) -> Reading {
        Reading {
            source: source.to_owned(),
            ts: 1_758_326_400_000,
            values: BTreeMap::from([(key.to_owned(), value)]),
        }
    }

    fn declared() -> Vec<Reading> {
        vec![reading("inverter", "power_w", 27_034)]
    }

    fn seqs(buffer: &Buffer) -> Vec<String> {
        buffer
            .state()
            .batches
            .iter()
            .map(|b| b.seq.clone())
            .collect()
    }

    #[test]
    fn the_counter_starts_at_zero_and_advances_once_per_batch() {
        let buffer = buffer(8);
        buffer.fill(4);
        assert_eq!(seqs(&buffer), ["0", "1", "2", "3"]);
    }

    #[test]
    fn every_batch_of_one_run_names_the_same_boot() {
        // Half of what identifies a batch. A boot id that changed between batches would make one
        // run look like several and every batch a restart.
        let buffer = buffer(4);
        buffer.fill(2);
        let boots: Vec<String> = buffer
            .state()
            .batches
            .iter()
            .map(|b| b.boot_id.clone())
            .collect();
        assert_eq!(boots, ["0123456789abcdef", "0123456789abcdef"]);
    }

    #[test]
    fn a_full_buffer_drops_the_oldest_not_the_newest() {
        let buffer = buffer(3);
        buffer.fill(5);
        // The newest readings are the ones worth keeping: the oldest are the likeliest to be
        // stale by the time a connection comes back.
        assert_eq!(seqs(&buffer), ["2", "3", "4"]);
    }

    #[test]
    fn the_dropped_batches_leave_a_visible_seq_gap() {
        // Invariant 7. The device cannot tell the cloud it lost something, so the loss has to be
        // legible in what it does send: 0, 1, then 5 — three numbers spent and never delivered.
        let buffer = buffer(2);
        buffer.fill(2);
        let mut delivered = Vec::new();
        while let Some(sent) = buffer.front() {
            assert!(buffer.release(&sent));
            delivered.push(sent.seq);
        }
        buffer.fill(4);
        assert_eq!(delivered, ["0", "1"]);
        assert_eq!(seqs(&buffer), ["4", "5"], "2 and 3 were never delivered");
    }

    #[test]
    fn an_overflowing_enqueue_names_what_it_dropped() {
        // The journal names the lost `seq`; the cloud only ever sees the gap.
        let buffer = buffer(1);
        let first = buffer.enqueue(declared());
        let first = first.queued.expect("declared, so queued");
        assert_eq!(first.displaced, None, "nothing dropped yet");
        let second = buffer.enqueue(declared());
        let second = second.queued.expect("declared, so queued");
        assert_eq!(second.displaced.as_deref(), Some("0"));
        assert_eq!(second.dropped, 1);
        assert_eq!(
            seqs(&buffer),
            ["1"],
            "a buffer of one holds only the newest"
        );
    }

    #[test]
    fn depth_is_what_is_queued() {
        let buffer = buffer(4);
        assert_eq!(buffer.depth(), 0);
        buffer.fill(2);
        assert_eq!(buffer.depth(), 2);
        let front = buffer.front().expect("two queued");
        assert!(buffer.release(&front));
        assert_eq!(buffer.depth(), 1);
    }

    #[test]
    fn a_reading_the_manifest_does_not_declare_is_refused_and_the_rest_kept() {
        // One driver emitting a stray key must not cost the other sources their sweep.
        let buffer = buffer(4);
        let enqueued = buffer.enqueue(vec![
            reading("inverter", "power_w", 27_034),
            reading("meter", "power_w", 1),
            reading("inverter", "voltage_v", 230),
        ]);
        let refused: Vec<(&str, Refusal)> = enqueued
            .refused
            .iter()
            .map(|(source, why)| (source.as_str(), why.clone()))
            .collect();
        assert_eq!(
            refused,
            [
                (
                    "meter",
                    Refusal::Undeclared {
                        source: "meter".to_owned(),
                        metric: None
                    }
                ),
                (
                    "inverter",
                    Refusal::Undeclared {
                        source: "inverter".to_owned(),
                        metric: Some("voltage_v".to_owned())
                    }
                ),
            ]
        );
        let kept = buffer.front().expect("one reading was declared");
        assert_eq!(kept.readings, [reading("inverter", "power_w", 27_034)]);
    }

    #[test]
    fn a_value_past_the_contract_is_refused() {
        // A signed 64-bit value reaches past what the cloud accepts: the type is not the bound.
        let buffer = buffer(4);
        let past = contract::limits::METRIC_VALUE_MAX + 1;
        let enqueued = buffer.enqueue(vec![reading("inverter", "power_w", past)]);
        assert_eq!(enqueued.refused.len(), 1);
        assert_eq!(enqueued.queued, None);
    }

    #[test]
    fn a_sweep_refused_whole_spends_no_seq() {
        // Nothing stamped is nothing lost from the sequence: a gap would claim a batch that never
        // existed.
        let buffer = buffer(4);
        let refused = buffer.enqueue(vec![reading("meter", "power_w", 1)]);
        assert_eq!(refused.queued, None);
        assert_eq!(buffer.depth(), 0);
        buffer.fill(1);
        assert_eq!(seqs(&buffer), ["0"]);
    }

    #[test]
    fn what_is_queued_the_cloud_would_take() {
        let buffer = buffer(4);
        buffer.fill(1);
        let batch = buffer.front().expect("one queued");
        assert_eq!(batch.validate(), Ok(()));
        assert_eq!(batch.check_against(&buffer.manifest), Ok(()));
    }

    #[test]
    fn front_reads_the_oldest_and_release_removes_it() {
        let buffer = buffer(4);
        buffer.fill(2);
        let front = buffer.front().expect("two queued");
        assert_eq!(front.seq, "0");
        assert_eq!(
            buffer.front().map(|b| b.seq),
            Some("0".to_owned()),
            "front removed it"
        );
        assert!(buffer.release(&front));
        assert_eq!(buffer.front().map(|b| b.seq), Some("1".to_owned()));
    }

    #[test]
    fn releasing_from_an_empty_buffer_is_not_an_error() {
        // A drain that races an empty buffer must not be the thing that takes the device down.
        let buffer = buffer(2);
        buffer.fill(1);
        let sent = buffer.front().expect("one queued");
        assert!(buffer.release(&sent));
        assert!(!buffer.release(&sent));
        assert_eq!(buffer.depth(), 0);
    }

    #[test]
    fn a_batch_overflowed_out_mid_flight_is_not_released_twice() {
        // The poll overflowed the buffer while `sent` was in flight, so `sent` is already gone.
        // Releasing on the answer would drop a batch that never reached the cloud.
        let buffer = buffer(2);
        buffer.fill(1);
        let sent = buffer.front().expect("one queued");
        buffer.fill(3);
        assert!(!buffer.release(&sent), "the wrong batch was released");
        assert_eq!(seqs(&buffer), ["2", "3"]);
    }

    #[test]
    fn a_poisoned_lock_still_guards_a_usable_buffer() {
        let buffer = buffer(2);
        buffer.fill(1);
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = buffer.state.lock();
            panic!("a task died holding the buffer");
        }));
        assert!(poisoned.is_err());
        assert!(buffer.state.is_poisoned());
        buffer.fill(1);
        assert_eq!(buffer.depth(), 2);
        assert!(buffer.front().is_some_and(|sent| buffer.release(&sent)));
    }
}
