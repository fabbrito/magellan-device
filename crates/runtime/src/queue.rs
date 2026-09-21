//! The bounded queue of batches awaiting upload.

use std::collections::VecDeque;
use std::num::NonZeroUsize;

use contract::Batch;

use crate::Buffer;

/// A bounded queue of batches, oldest first, held in RAM.
///
/// Full, it drops the oldest batch rather than growing into the memory the rest of the device
/// needs. The dropped batch already carries its `seq`, so what reaches the cloud has a visible
/// gap where it was — which is the point. A gap is a health signal; a device that died of a full
/// queue is not.
#[derive(Debug)]
pub struct Queue {
    batches: VecDeque<Batch>,
    capacity: NonZeroUsize,
    dropped: u64,
}

impl Queue {
    /// A queue holding at most `capacity` batches.
    ///
    /// Non-zero by the type: a queue that can hold nothing drops every reading the moment it is
    /// made, and would look like a working device doing it.
    #[must_use]
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            batches: VecDeque::with_capacity(capacity.get()),
            capacity,
            dropped: 0,
        }
    }

    /// Batches dropped to overflow over this queue's life. The journal's number; the cloud sees
    /// the same loss as a `seq` gap.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl Buffer for Queue {
    fn depth(&self) -> u32 {
        // Saturating: the contract bounds what it will accept, and a depth past `u32` means the
        // bound was never applied. Reporting the ceiling beats wrapping to nothing.
        u32::try_from(self.batches.len()).unwrap_or(u32::MAX)
    }

    fn push(&mut self, batch: Batch) -> Option<Batch> {
        // Full, the oldest goes to make room. The popped batch is the one the journal names: its
        // `seq` never reaches the cloud, so the loss shows there only as a gap.
        let dropped = if self.batches.len() >= self.capacity.get() {
            let dropped = self.batches.pop_front();
            self.dropped = self.dropped.saturating_add(1);
            dropped
        } else {
            None
        };
        self.batches.push_back(batch);
        dropped
    }

    fn peek(&self) -> Option<&Batch> {
        self.batches.front()
    }

    fn pop(&mut self) {
        self.batches.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(capacity: usize) -> Queue {
        Queue::new(NonZeroUsize::new(capacity).expect("a capacity of at least one"))
    }

    fn batch(seq: u64) -> Batch {
        Batch {
            manifest_hash: "0".repeat(64),
            boot_id: "0123456789abcdef".to_owned(),
            seq: seq.to_string(),
            readings: Vec::new(),
            heartbeat: None,
        }
    }

    fn seqs(queue: &Queue) -> Vec<&str> {
        queue.batches.iter().map(|b| b.seq.as_str()).collect()
    }

    #[test]
    fn a_full_queue_drops_the_oldest_not_the_newest() {
        let mut queue = queue(3);
        for seq in 1..=5 {
            queue.push(batch(seq));
        }
        // The newest readings are the ones worth keeping: the oldest are the likeliest to be
        // stale by the time a connection comes back.
        assert_eq!(seqs(&queue), ["3", "4", "5"]);
        assert_eq!(queue.dropped(), 2);
    }

    #[test]
    fn the_dropped_batches_leave_a_visible_seq_gap() {
        // Invariant 7. The device cannot tell the cloud it lost something, so the loss has to be
        // legible in what it does send: 1, 2, then 6 — four numbers spent and never delivered.
        let mut queue = queue(2);
        for seq in 1..=2 {
            queue.push(batch(seq));
        }
        let delivered = seqs(&queue).join(",");
        for seq in 3..=6 {
            queue.push(batch(seq));
        }
        assert_eq!(delivered, "1,2");
        assert_eq!(seqs(&queue), ["5", "6"]);
        assert_eq!(queue.dropped(), 4, "3 and 4 were never delivered either");
    }

    #[test]
    fn depth_is_what_the_heartbeat_reports() {
        let mut queue = queue(4);
        assert_eq!(queue.depth(), 0);
        queue.push(batch(1));
        queue.push(batch(2));
        assert_eq!(queue.depth(), 2);
        queue.pop();
        assert_eq!(queue.depth(), 1);
    }

    #[test]
    fn peek_reads_the_oldest_and_pop_removes_it() {
        let mut queue = queue(4);
        queue.push(batch(7));
        queue.push(batch(8));
        assert_eq!(queue.peek().map(|b| b.seq.as_str()), Some("7"));
        assert_eq!(
            queue.peek().map(|b| b.seq.as_str()),
            Some("7"),
            "peek removed it"
        );
        queue.pop();
        assert_eq!(queue.peek().map(|b| b.seq.as_str()), Some("8"));
    }

    #[test]
    fn popping_an_empty_queue_is_not_an_error() {
        // A drain that races an empty queue must not be the thing that takes the device down.
        let mut queue = queue(2);
        queue.pop();
        assert_eq!(queue.depth(), 0);
        assert!(queue.peek().is_none());
    }

    #[test]
    fn an_overflowing_push_names_what_it_dropped() {
        // The journal names the lost `seq`; the cloud only ever sees the gap.
        let mut queue = queue(1);
        assert!(queue.push(batch(1)).is_none(), "nothing dropped yet");
        assert_eq!(
            queue.push(batch(2)).map(|b| b.seq),
            Some("1".to_owned()),
            "the batch evicted is the one a journal line must name"
        );
    }

    #[test]
    fn a_queue_of_one_holds_only_the_newest() {
        let mut queue = queue(1);
        queue.push(batch(1));
        queue.push(batch(2));
        assert_eq!(seqs(&queue), ["2"]);
        assert_eq!(queue.depth(), 1);
    }
}
