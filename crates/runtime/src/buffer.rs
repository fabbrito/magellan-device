//! The bounded buffer of batches awaiting upload: stamped, queued and released under one lock.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::{Mutex, MutexGuard, PoisonError};

use contract::{Batch, Heartbeat, Reading};

/// A bounded, at-least-once queue of batches, oldest first, held in RAM and shared by the poll
/// and the drain.
///
/// Full, it drops the oldest batch rather than growing into the memory the rest of the device
/// needs. The dropped batch already carries its `seq`, so what reaches the cloud has a visible gap
/// where it was — a health signal, never something hidden (ADR 4).
///
/// The boot id is drawn once and the counter starts at zero, which together identify a batch
/// (ADR 8). Stamping and queueing share the lock, so `seq` order is queue order.
#[derive(Debug)]
pub struct Buffer {
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
    /// The `seq` stamped on the new batch.
    pub seq: String,
    /// The depth the batch's heartbeat reports.
    pub depth: u32,
    /// The `seq` pushed out the front to make room. It never reaches the cloud: the gap.
    pub displaced: Option<String>,
    /// Batches displaced over this buffer's life.
    pub dropped: u64,
}

impl Buffer {
    /// A buffer of at most `capacity` batches, stamping for the manifest the hash names, in the
    /// boot `boot_id` names.
    ///
    /// Non-zero by the type: a buffer that can hold nothing drops every reading the moment it is
    /// made, and would look like a working device doing it.
    #[must_use]
    pub fn new(capacity: NonZeroUsize, manifest_hash: String, boot_id: String) -> Self {
        Self {
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

    /// Stamp `readings` as the next batch and append it, dropping the oldest when full.
    ///
    /// `beat` is handed the depth to report. The `seq` is spent whether or not the batch is ever
    /// delivered — a number spent on a batch later dropped is exactly the gap that shows the loss.
    pub fn enqueue(&self, readings: Vec<Reading>, beat: impl FnOnce(u32) -> Heartbeat) -> Enqueued {
        let mut state = self.state();
        let depth = depth_of(&state.batches);
        let seq = state.seq.to_string();
        let batch = Batch {
            manifest_hash: self.manifest_hash.clone(),
            boot_id: self.boot_id.clone(),
            seq: seq.clone(),
            readings,
            heartbeat: beat(depth),
        };
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
            seq,
            depth,
            displaced,
            dropped: state.dropped,
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn buffer(capacity: usize) -> Buffer {
        Buffer::new(
            NonZeroUsize::new(capacity).expect("a capacity of at least one"),
            "0".repeat(64),
            "0123456789abcdef".to_owned(),
        )
    }

    fn reading() -> Vec<Reading> {
        vec![Reading {
            source: "inverter".to_owned(),
            ts: 1_758_326_400_000,
            values: BTreeMap::from([("power_w".to_owned(), 27_034_i64)]),
        }]
    }

    fn fill(buffer: &Buffer, batches: usize) {
        for _ in 0..batches {
            buffer.enqueue(reading(), |depth| Heartbeat::new(1, depth));
        }
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
    fn a_stamped_batch_satisfies_the_contract() {
        let buffer = buffer(4);
        fill(&buffer, 1);
        assert_eq!(buffer.front().map(|b| b.validate()), Some(Ok(())));
    }

    #[test]
    fn the_counter_starts_at_zero_and_advances_once_per_batch() {
        let buffer = buffer(8);
        fill(&buffer, 4);
        assert_eq!(seqs(&buffer), ["0", "1", "2", "3"]);
    }

    #[test]
    fn every_batch_of_one_run_names_the_same_boot() {
        // Half of what identifies a batch. A boot id that changed between batches would make one
        // run look like several and every batch a restart.
        let buffer = buffer(4);
        fill(&buffer, 2);
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
        fill(&buffer, 5);
        // The newest readings are the ones worth keeping: the oldest are the likeliest to be
        // stale by the time a connection comes back.
        assert_eq!(seqs(&buffer), ["2", "3", "4"]);
    }

    #[test]
    fn the_dropped_batches_leave_a_visible_seq_gap() {
        // Invariant 7. The device cannot tell the cloud it lost something, so the loss has to be
        // legible in what it does send: 0, 1, then 5 — three numbers spent and never delivered.
        let buffer = buffer(2);
        fill(&buffer, 2);
        let mut delivered = Vec::new();
        while let Some(sent) = buffer.front() {
            assert!(buffer.release(&sent));
            delivered.push(sent.seq);
        }
        fill(&buffer, 4);
        assert_eq!(delivered, ["0", "1"]);
        assert_eq!(seqs(&buffer), ["4", "5"], "2 and 3 were never delivered");
    }

    #[test]
    fn an_overflowing_enqueue_names_what_it_dropped() {
        // The journal names the lost `seq`; the cloud only ever sees the gap.
        let buffer = buffer(1);
        let first = buffer.enqueue(reading(), |depth| Heartbeat::new(1, depth));
        assert_eq!(first.displaced, None, "nothing dropped yet");
        let second = buffer.enqueue(reading(), |depth| Heartbeat::new(1, depth));
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
        fill(&buffer, 2);
        assert_eq!(buffer.depth(), 2);
        let front = buffer.front().expect("two queued");
        assert!(buffer.release(&front));
        assert_eq!(buffer.depth(), 1);
    }

    #[test]
    fn front_reads_the_oldest_and_release_removes_it() {
        let buffer = buffer(4);
        fill(&buffer, 2);
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
        fill(&buffer, 1);
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
        fill(&buffer, 1);
        let sent = buffer.front().expect("one queued");
        fill(&buffer, 3);
        assert!(!buffer.release(&sent), "the wrong batch was released");
        assert_eq!(seqs(&buffer), ["2", "3"]);
    }

    #[test]
    fn a_poisoned_lock_still_guards_a_usable_buffer() {
        let buffer = buffer(2);
        fill(&buffer, 1);
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = buffer.state.lock();
            panic!("a task died holding the buffer");
        }));
        assert!(poisoned.is_err());
        assert!(buffer.state.is_poisoned());
        fill(&buffer, 1);
        assert_eq!(buffer.depth(), 2);
        assert!(buffer.front().is_some_and(|sent| buffer.release(&sent)));
    }
}
