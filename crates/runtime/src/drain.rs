//! Draining the buffer, oldest first.

use std::sync::Mutex;

use tracing::{debug, warn};

use crate::{Buffer, Cloud, Outcome, Queue};

/// Send the oldest batch and act on the answer. `None` when there was nothing to send.
///
/// The batch is copied out from under the lock before the request and never held across it: a
/// drain that kept the lock for a round trip would stall the poll it exists to run beside, which
/// is the one thing `DESIGN.md` §7 asks of this pair.
pub async fn drain_once(buffer: &Mutex<Queue>, cloud: &dyn Cloud) -> Option<Outcome> {
    // A poisoned lock means another task panicked holding it. Draining nothing is the safe
    // reading of that; the panic is the thing to fix, and it is already in the journal.
    let batch = buffer.lock().ok()?.peek().cloned()?;
    let outcome = cloud.send(&batch).await;
    // Where the batch's fate is known: the journal names the `seq`, and the cloud shows the rest.
    match outcome {
        Outcome::Committed => debug!(seq = %batch.seq, "batch committed"),
        Outcome::Rejected(status) => {
            warn!(seq = %batch.seq, status, "the cloud rejected a batch; dropped");
        }
        Outcome::Credential | Outcome::Unavailable => {}
    }
    if outcome.releases_the_batch()
        && let Ok(mut queue) = buffer.lock()
    {
        // While the request was in flight the poll may have filled the queue and dropped this
        // batch to overflow, putting a different one at the front. Popping blindly would drop a
        // batch that was never sent — a reading lost with no `seq` gap to show for it.
        let still_ours = queue
            .peek()
            .is_some_and(|front| front.boot_id == batch.boot_id && front.seq == batch.seq);
        if still_ours {
            queue.pop();
        }
    }
    Some(outcome)
}

#[cfg(all(test, feature = "fake"))]
mod tests {
    use std::num::NonZeroUsize;

    use async_trait::async_trait;
    use contract::{Batch, Manifest};

    use super::*;
    use crate::Declined;
    use crate::upload::fake::Fake;

    fn queue(capacity: usize) -> Mutex<Queue> {
        // Helpers beside the tests do not get the lint's test exemption, and a capacity of
        // zero is a test that meant something else anyway.
        Mutex::new(Queue::new(
            NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN),
        ))
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

    fn front(buffer: &Mutex<Queue>) -> Option<String> {
        buffer.lock().ok()?.peek().map(|b| b.seq.clone())
    }

    #[tokio::test]
    async fn a_committed_batch_leaves_the_buffer() {
        let buffer = queue(4);
        buffer.lock().unwrap().push(batch(1));
        buffer.lock().unwrap().push(batch(2));
        let outcome = drain_once(&buffer, &Fake::always(Outcome::Committed)).await;
        assert_eq!(outcome, Some(Outcome::Committed));
        assert_eq!(front(&buffer).as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn a_rejected_batch_leaves_too_rather_than_blocking_the_queue_forever() {
        let buffer = queue(4);
        buffer.lock().unwrap().push(batch(1));
        let outcome = drain_once(&buffer, &Fake::always(Outcome::Rejected(422))).await;
        assert_eq!(outcome, Some(Outcome::Rejected(422)));
        assert!(
            front(&buffer).is_none(),
            "a rejected batch must not be retried forever"
        );
    }

    #[tokio::test]
    async fn an_unavailable_cloud_keeps_the_batch() {
        let buffer = queue(4);
        buffer.lock().unwrap().push(batch(1));
        assert_eq!(
            drain_once(&buffer, &Fake::always(Outcome::Unavailable)).await,
            Some(Outcome::Unavailable)
        );
        assert_eq!(front(&buffer).as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn a_refused_credential_keeps_the_batch() {
        // A rotated token must not cost the readings taken while it was stale.
        let buffer = queue(4);
        buffer.lock().unwrap().push(batch(1));
        assert_eq!(
            drain_once(&buffer, &Fake::always(Outcome::Credential)).await,
            Some(Outcome::Credential)
        );
        assert_eq!(front(&buffer).as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn an_empty_buffer_is_not_an_error() {
        let buffer = queue(4);
        assert_eq!(
            drain_once(&buffer, &Fake::always(Outcome::Committed)).await,
            None
        );
    }

    #[tokio::test]
    async fn an_outage_then_recovery_drains_in_order() {
        // The shape §2 is about, exercised whole rather than one branch at a time.
        let buffer = queue(8);
        for seq in 1..=3 {
            buffer.lock().unwrap().push(batch(seq));
        }
        let cloud = Fake::answering(
            vec![Outcome::Unavailable, Outcome::Unavailable],
            Outcome::Committed,
        );
        for _ in 0..2 {
            drain_once(&buffer, &cloud).await;
            assert_eq!(
                front(&buffer).as_deref(),
                Some("1"),
                "nothing leaves during an outage"
            );
        }
        for expected in ["2", "3"] {
            drain_once(&buffer, &cloud).await;
            assert_eq!(front(&buffer).as_deref(), Some(expected));
        }
        drain_once(&buffer, &cloud).await;
        assert!(front(&buffer).is_none(), "the buffer drained");
    }

    /// A cloud that overflows the queue while the request is in flight, which is what the poll
    /// does to a full buffer during a slow upload.
    struct Overflowing<'a>(&'a Mutex<Queue>);

    #[async_trait]
    impl Cloud for Overflowing<'_> {
        async fn declare(&self, _manifest: &Manifest) -> Result<String, Declined> {
            Ok(String::new())
        }

        async fn send(&self, _batch: &Batch) -> Outcome {
            if let Ok(mut queue) = self.0.lock() {
                for seq in 10..=12 {
                    queue.push(batch(seq));
                }
            }
            Outcome::Committed
        }
    }

    #[tokio::test]
    async fn a_batch_dropped_to_overflow_mid_flight_is_not_popped_twice() {
        // The batch that was sent is already gone, pushed out by the poll. Popping on the answer
        // would take a batch that never reached the cloud — a reading lost with no gap to show.
        let buffer = queue(2);
        buffer.lock().unwrap().push(batch(1));
        let cloud = Overflowing(&buffer);
        let outcome = drain_once(&buffer, &cloud).await;
        assert_eq!(outcome, Some(Outcome::Committed));
        // Capacity 2, so 11 and 12 survive: seq 1 was dropped to overflow and 10 with it.
        assert_eq!(
            front(&buffer).as_deref(),
            Some("11"),
            "the wrong batch was popped"
        );
        assert_eq!(buffer.lock().unwrap().depth(), 2);
    }
}
