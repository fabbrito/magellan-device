//! Draining the buffer, oldest first.

use std::sync::Mutex;

use contract::Manifest;
use tokio::time::sleep;
use tracing::{debug, warn};

use crate::backoff::Backoff;
use crate::{Buffer, Cadence, Cloud, Declined, Outcome, Queue};

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

/// Declare the manifest until the cloud takes it, backing off while it cannot.
///
/// The cloud is not required at start: a device booting during an outage polls and buffers, and
/// this asks again on the cadence a later outage is met with. Only an answer that trying again
/// cannot fix returns — a permanent refusal, or an `ETag` the device did not compute.
///
/// # Errors
///
/// [`Declined`] when asking again cannot help.
pub async fn declare_forever(
    cloud: &dyn Cloud,
    manifest: &Manifest,
    cadence: Cadence,
) -> Result<(), Declined> {
    let mut backoff = Backoff::new(cadence.backoff_min, cadence.backoff_max);
    loop {
        match cloud.declare(manifest).await {
            Ok(_accepted) => return Ok(()),
            // The cloud cannot answer now. It will not have gotten better by asking at once.
            Err(Declined::Answer(Outcome::Unavailable | Outcome::Credential)) => {}
            Err(declined) => return Err(declined),
        }
        // The drain's ladder, and it matters more here: a street's power coming back boots a
        // fleet at once, and every device declares before it sends anything.
        let wait = backoff.climb();
        warn!(?wait, "the cloud is not taking the manifest yet");
        sleep(wait).await;
    }
}

#[cfg(all(test, feature = "fake"))]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicU32, Ordering};

    use async_trait::async_trait;
    use contract::{Batch, Manifest};

    use super::*;
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
            heartbeat: contract::Heartbeat::new(1, 0),
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

    /// Fast enough that a test is about the retry, not the wait.
    fn cadence() -> Cadence {
        Cadence {
            sweep: std::time::Duration::from_secs(1),
            backoff_min: std::time::Duration::from_millis(1),
            backoff_max: std::time::Duration::from_millis(2),
            drain_pace: std::time::Duration::from_millis(1),
            recheck: std::time::Duration::from_secs(1),
        }
    }

    fn manifest() -> Manifest {
        Manifest {
            sources: Vec::new(),
        }
    }

    /// Refuses the manifest a fixed number of times, then agrees.
    struct Reluctant {
        refusals: AtomicU32,
    }

    #[async_trait]
    impl Cloud for Reluctant {
        async fn declare(&self, manifest: &Manifest) -> Result<String, Declined> {
            let refused = self
                .refusals
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                .is_ok();
            if refused {
                return Err(Declined::Answer(Outcome::Unavailable));
            }
            crate::manifest_hash(manifest)
        }

        async fn send(&self, _batch: &Batch) -> Outcome {
            Outcome::Committed
        }
    }

    #[tokio::test]
    async fn a_cloud_down_at_boot_is_asked_again_until_it_takes_the_manifest() {
        // The quirk this fixes: starting used to fail on the first `Unavailable`, so a boot during
        // an outage lost every reading taken before the cloud came back.
        let cloud = Reluctant {
            refusals: AtomicU32::new(2),
        };
        declare_forever(&cloud, &manifest(), cadence())
            .await
            .expect("the third ask is taken");
        // Both refusals were spent, so it was asked a third time to have succeeded at all.
        assert_eq!(cloud.refusals.load(Ordering::Relaxed), 0);
    }

    /// A cloud whose manifest answer is always this.
    struct Refusing(Outcome);

    #[async_trait]
    impl Cloud for Refusing {
        async fn declare(&self, _manifest: &Manifest) -> Result<String, Declined> {
            Err(Declined::Answer(self.0))
        }

        async fn send(&self, _batch: &Batch) -> Outcome {
            self.0
        }
    }

    #[tokio::test]
    async fn a_permanent_refusal_ends_the_run_rather_than_looping() {
        // A manifest the cloud will never take is not an outage: retrying fills the buffer and
        // then starts dropping readings, which is worse than stopping loudly.
        let cloud = Refusing(Outcome::Rejected(422));
        assert_eq!(
            declare_forever(&cloud, &manifest(), cadence()).await,
            Err(Declined::Answer(Outcome::Rejected(422)))
        );
    }
}
