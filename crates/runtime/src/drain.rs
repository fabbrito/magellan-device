//! Draining the buffer, oldest first — one attempt, the loop around it, and the manifest those
//! batches name, declared before any of them go.

use std::sync::Arc;
use std::time::Duration;

use contract::Encoded;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::backoff::Backoff;
use crate::{Buffer, Cadence, Cloud, Declined, Outcome};

/// Send the oldest batch and act on the answer. `None` when there was nothing to send.
///
/// The batch is copied out from under the lock before the request and never held across it: a
/// drain that kept the lock for a round trip would stall the poll it exists to run beside, which
/// is the one thing `DESIGN.md` §7 asks of this pair.
pub async fn drain_once(buffer: &Buffer, cloud: &dyn Cloud) -> Option<Outcome> {
    let batch = buffer.front()?;
    let outcome = cloud.send(&batch).await;
    // Where the batch's fate is known: the journal names the `seq`, and the cloud shows the rest.
    match outcome {
        Outcome::Committed => debug!(seq = %batch.seq, "batch committed"),
        Outcome::Rejected(status) => {
            warn!(seq = %batch.seq, status, "the cloud rejected a batch; dropped");
        }
        Outcome::Credential | Outcome::Unavailable => {}
    }
    if !outcome.retries() {
        buffer.release(&batch);
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
    manifest: &Encoded,
    cadence: Cadence,
    seed: u64,
) -> Result<(), Declined> {
    let mut backoff = Backoff::seeded(cadence.backoff_first, cadence.backoff_ceiling, seed);
    loop {
        match cloud.declare(manifest).await {
            Ok(_accepted) => return Ok(()),
            // The cloud cannot answer now. It will not have gotten better by asking at once.
            Err(Declined::Answer(answer)) if answer.retries() => {}
            Err(declined) => return Err(declined),
        }
        // The drain's backoff, and it matters more here: a street's power coming back boots a
        // fleet at once, and every device declares before it sends anything.
        let wait = backoff.next_wait();
        warn!(?wait, "the cloud is not taking the manifest yet");
        sleep(wait).await;
    }
}

/// Drain the buffer until it is empty or the cloud stops taking batches, backing off as it goes;
/// at a stop, flush what is left.
///
/// Runs beside the poll rather than inside it, which is what keeps `DESIGN.md` §7's "polling
/// never waits on the network" true by construction rather than by care.
pub async fn drain_forever(
    buffer: Arc<Buffer>,
    cloud: Arc<dyn Cloud>,
    cadence: Cadence,
    seed: u64,
    stop: CancellationToken,
) {
    let draining = async {
        drain_until(&buffer, cloud.as_ref(), cadence, seed, &stop).await;
        flush(&buffer, cloud.as_ref(), cadence.drain_pace).await;
    };
    // One deadline over the send in flight at the stop and the flush after it, so the whole of
    // stopping ends before the service manager kills. A send cut off keeps its batch.
    let deadline = async {
        stop.cancelled().await;
        sleep(cadence.flush).await;
    };
    tokio::select! {
        () = draining => {}
        () = deadline => {}
    }
}

/// The drain proper, until stopped.
async fn drain_until(
    buffer: &Buffer,
    cloud: &dyn Cloud,
    cadence: Cadence,
    seed: u64,
    stop: &CancellationToken,
) {
    let mut backoff = Backoff::seeded(cadence.backoff_first, cadence.backoff_ceiling, seed);
    let mut refusing = false;
    while !stop.is_cancelled() {
        // `drain_once` journalled the batch's fate, `seq` and all; what is left is the refusal,
        // how long the device stops asking for, and the end of it.
        let outcome = drain_once(buffer, cloud).await;
        let wait = drain_next_wait(outcome, cadence, &mut backoff);
        match outcome {
            Some(answer) if answer.retries() => {
                refusing = true;
                warn!(outcome = ?answer, ?wait, "the cloud is not taking batches");
            }
            // A state change, so `info`: without it the journal shows an outage that never ends.
            Some(_) if refusing => {
                refusing = false;
                info!("the cloud is taking batches again");
            }
            _ => {}
        }
        tokio::select! {
            () = sleep(wait) => {}
            () = stop.cancelled() => {}
        }
    }
}

/// Send what the buffer still holds, at pace, until it is empty or the cloud says to wait — a
/// stopping device cannot. The buffer is RAM (ADR 4): what is left after is lost.
async fn flush(buffer: &Buffer, cloud: &dyn Cloud, pace: Duration) {
    let depth = buffer.depth();
    if depth == 0 {
        return;
    }
    info!(depth, "flushing the buffer before stopping");
    while let Some(outcome) = drain_once(buffer, cloud).await {
        if outcome.retries() || buffer.depth() == 0 {
            return;
        }
        sleep(pace).await;
    }
}

/// How long to wait after one drain attempt, moving the backoff with it. Outside the loop, so
/// what an answer costs is tested without sleeping through it.
fn drain_next_wait(outcome: Option<Outcome>, cadence: Cadence, backoff: &mut Backoff) -> Duration {
    match outcome {
        // Nothing queued: wait for the poll to make something.
        None => {
            backoff.reset();
            cadence.backoff_first
        }
        // Taking batches: keep going while it does, at a pace rather than a burst.
        Some(outcome) if !outcome.retries() => {
            backoff.reset();
            cadence.drain_pace
        }
        // Refused: wait out this interval, and double the next.
        Some(_) => backoff.next_wait(),
    }
}

#[cfg(test)]
mod tests {

    use async_trait::async_trait;
    use contract::{Batch, Encoded, Manifest};

    use super::*;
    use crate::upload::fake::Fake;

    /// Intervals told apart at a glance, and short enough that a test waiting one is about the
    /// retry rather than the wait: 8ms, doubling to a 32ms ceiling.
    fn cadence() -> Cadence {
        Cadence {
            sweep: Duration::from_secs(300),
            backoff_first: Duration::from_millis(8),
            backoff_ceiling: Duration::from_millis(32),
            recheck: Duration::from_mins(15),
            heartbeat: Duration::from_hours(1),
            drain_pace: Duration::from_millis(1),
            flush: Duration::from_secs(60),
        }
    }

    /// A backoff already at its ceiling, which is where a reset is visible.
    fn exhausted(cadence: Cadence) -> Backoff {
        let mut backoff = Backoff::seeded(cadence.backoff_first, cadence.backoff_ceiling, 1);
        for _ in 0..4 {
            backoff.next_wait();
        }
        backoff
    }

    #[test]
    fn a_cloud_taking_batches_is_paced_rather_than_burst() {
        // Draining as fast as the link allows is a spike at a cloud that has just come back.
        let cadence = cadence();
        for outcome in [Outcome::Committed, Outcome::Rejected(422)] {
            let mut backoff = exhausted(cadence);
            let wait = drain_next_wait(Some(outcome), cadence, &mut backoff);
            assert_eq!(wait, cadence.drain_pace, "the backlog burst");
            let next = drain_next_wait(Some(Outcome::Unavailable), cadence, &mut backoff);
            assert!(
                next < cadence.backoff_first,
                "the backoff did not reset: {next:?}"
            );
        }
    }

    #[test]
    fn a_refusal_backs_off_rather_than_asking_again_at_once() {
        let cadence = cadence();
        // Spread, so there is no sequence to assert on: each wait belongs to its own interval.
        let mut backoff = Backoff::seeded(cadence.backoff_first, cadence.backoff_ceiling, 1);
        let waits: Vec<Duration> = (0..5)
            .map(|_| drain_next_wait(Some(Outcome::Unavailable), cadence, &mut backoff))
            .collect();
        assert!(
            waits
                .iter()
                .all(|wait| *wait >= cadence.backoff_first / 2 && *wait < cadence.backoff_ceiling),
            "a wait left the backoff: {waits:?}"
        );
        let last = waits.last().copied().unwrap_or_default();
        assert!(
            last >= cadence.backoff_ceiling / 2,
            "five refusals did not reach the ceiling: {last:?}"
        );
    }

    /// How long the whole loop took to empty a buffer of `batches`, on paused time: every sleep
    /// is skipped, and the clock still says how long each one was.
    async fn drained_after(batches: u64, cloud: Fake, cadence: Cadence) -> Duration {
        let buffer = Arc::new(buffer(8));
        buffer.fill(batches);
        let cloud: Arc<dyn Cloud> = Arc::new(cloud);
        let stop = CancellationToken::new();
        let started = tokio::time::Instant::now();
        let drain = tokio::spawn(drain_forever(
            Arc::clone(&buffer),
            cloud,
            cadence,
            1,
            stop.clone(),
        ));
        let drained = loop {
            if buffer.depth() == 0 {
                break started.elapsed();
            }
            sleep(Duration::from_millis(10)).await;
        };
        stop.cancel();
        let stopped = tokio::time::timeout(Duration::from_secs(1), drain).await;
        assert!(
            matches!(stopped, Ok(Ok(()))),
            "the drain did not stop when told"
        );
        drained
    }

    #[tokio::test(start_paused = true)]
    async fn a_backlog_drains_at_its_pace() {
        let cadence = Cadence {
            drain_pace: Duration::from_secs(1),
            ..cadence()
        };
        let took = drained_after(3, Fake::always(Outcome::Committed), cadence).await;
        // Sent at 0s, 1s and 2s. A burst empties the buffer at once.
        assert!(took >= Duration::from_secs(2), "burst: {took:?}");
        assert!(
            took < Duration::from_secs(3),
            "slower than its pace: {took:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_outage_is_waited_out_then_drained() {
        let cadence = Cadence {
            backoff_first: Duration::from_secs(8),
            backoff_ceiling: Duration::from_secs(32),
            ..cadence()
        };
        let cloud = Fake::answering(
            vec![Outcome::Unavailable, Outcome::Unavailable],
            Outcome::Committed,
        );
        let took = drained_after(1, cloud, cadence).await;
        // Two spread waits, each at least half its interval (8s, then 16s) and under all of it.
        assert!(
            took >= Duration::from_secs(12),
            "did not back off: {took:?}"
        );
        assert!(
            took < Duration::from_secs(25),
            "waited past its intervals: {took:?}"
        );
    }

    /// Run the drain already told to stop, and say how long its flush took.
    async fn flushed(
        buffer: &Arc<Buffer>,
        cloud: impl Cloud + 'static,
        cadence: Cadence,
    ) -> Duration {
        let stop = CancellationToken::new();
        stop.cancel();
        let started = tokio::time::Instant::now();
        drain_forever(Arc::clone(buffer), Arc::new(cloud), cadence, 1, stop).await;
        started.elapsed()
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_flushes_what_the_buffer_holds_at_its_pace() {
        // The buffer is RAM: what a stop leaves in it is lost.
        let cadence = Cadence {
            drain_pace: Duration::from_secs(1),
            ..cadence()
        };
        let buffer = Arc::new(Buffer::fixture(8));
        buffer.fill(3);
        let took = flushed(&buffer, Fake::always(Outcome::Committed), cadence).await;
        assert_eq!(buffer.depth(), 0, "the flush left batches behind");
        // Sent at 0s, 1s and 2s: a fleet stopped at once is still no burst.
        assert_eq!(took, Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn a_flush_ends_when_the_cloud_says_wait() {
        // A stopping device cannot wait out a backoff; asking again at once is the spike.
        let buffer = Arc::new(Buffer::fixture(8));
        buffer.fill(3);
        let took = flushed(&buffer, Fake::always(Outcome::Unavailable), cadence()).await;
        assert_eq!(buffer.depth(), 3);
        assert_eq!(took, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_flush_ends_at_its_deadline_send_in_flight_included() {
        // The service manager kills past its own timeout, and a kill journals nothing.
        let cadence = Cadence {
            flush: Duration::from_secs(60),
            ..cadence()
        };
        let buffer = Arc::new(Buffer::fixture(8));
        buffer.fill(1);
        let stop = CancellationToken::new();
        let drain = tokio::spawn(drain_forever(
            Arc::clone(&buffer),
            Arc::new(Hanging),
            cadence,
            1,
            stop.clone(),
        ));
        // The send is in flight, and will never come back.
        sleep(Duration::from_secs(1)).await;
        stop.cancel();
        let started = tokio::time::Instant::now();
        let stopped = tokio::time::timeout(Duration::from_secs(61), drain).await;
        assert!(
            matches!(stopped, Ok(Ok(()))),
            "the flush outlived its deadline"
        );
        assert_eq!(started.elapsed(), cadence.flush);
        assert_eq!(buffer.depth(), 1, "an unanswered batch is kept");
    }

    /// A cloud that never answers a batch.
    struct Hanging;

    #[async_trait]
    impl Cloud for Hanging {
        async fn declare(&self, _manifest: &Encoded) -> Result<String, Declined> {
            Ok(String::new())
        }

        async fn send(&self, _batch: &Batch) -> Outcome {
            std::future::pending().await
        }

        async fn beat(&self, _heartbeat: &contract::Heartbeat) -> Outcome {
            Outcome::Committed
        }
    }

    #[test]
    fn an_empty_buffer_looks_again_rather_than_backing_off() {
        // Nothing queued is not the cloud refusing: the next sweep is what this waits for.
        let cadence = cadence();
        let mut backoff = exhausted(cadence);
        assert_eq!(
            drain_next_wait(None, cadence, &mut backoff),
            cadence.backoff_first
        );
    }

    fn buffer(capacity: usize) -> Buffer {
        Buffer::fixture(capacity)
    }

    fn front(buffer: &Buffer) -> Option<String> {
        buffer.front().map(|b| b.seq)
    }

    #[tokio::test]
    async fn a_committed_batch_leaves_the_buffer() {
        let buffer = buffer(4);
        buffer.fill(2);
        let outcome = drain_once(&buffer, &Fake::always(Outcome::Committed)).await;
        assert_eq!(outcome, Some(Outcome::Committed));
        assert_eq!(front(&buffer).as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn a_rejected_batch_leaves_too_rather_than_blocking_the_queue_forever() {
        let buffer = buffer(4);
        buffer.fill(1);
        let outcome = drain_once(&buffer, &Fake::always(Outcome::Rejected(422))).await;
        assert_eq!(outcome, Some(Outcome::Rejected(422)));
        assert!(
            front(&buffer).is_none(),
            "a rejected batch must not be retried forever"
        );
    }

    #[tokio::test]
    async fn an_unavailable_cloud_keeps_the_batch() {
        let buffer = buffer(4);
        buffer.fill(1);
        assert_eq!(
            drain_once(&buffer, &Fake::always(Outcome::Unavailable)).await,
            Some(Outcome::Unavailable)
        );
        assert_eq!(front(&buffer).as_deref(), Some("0"));
    }

    #[tokio::test]
    async fn a_refused_credential_keeps_the_batch() {
        // A rotated token must not cost the readings taken while it was stale.
        let buffer = buffer(4);
        buffer.fill(1);
        assert_eq!(
            drain_once(&buffer, &Fake::always(Outcome::Credential)).await,
            Some(Outcome::Credential)
        );
        assert_eq!(front(&buffer).as_deref(), Some("0"));
    }

    #[tokio::test]
    async fn an_empty_buffer_is_not_an_error() {
        let buffer = buffer(4);
        assert_eq!(
            drain_once(&buffer, &Fake::always(Outcome::Committed)).await,
            None
        );
    }

    #[tokio::test]
    async fn an_outage_then_recovery_drains_in_order() {
        // The shape §2 is about, exercised whole rather than one branch at a time.
        let buffer = buffer(8);
        buffer.fill(3);
        let cloud = Fake::answering(
            vec![Outcome::Unavailable, Outcome::Unavailable],
            Outcome::Committed,
        );
        for _ in 0..2 {
            drain_once(&buffer, &cloud).await;
            assert_eq!(
                front(&buffer).as_deref(),
                Some("0"),
                "nothing leaves during an outage"
            );
        }
        for expected in ["1", "2"] {
            drain_once(&buffer, &cloud).await;
            assert_eq!(front(&buffer).as_deref(), Some(expected));
        }
        drain_once(&buffer, &cloud).await;
        assert!(front(&buffer).is_none(), "the buffer drained");
    }

    fn manifest() -> Encoded {
        Manifest {
            tz: "UTC".to_owned(),
            sources: vec![contract::Source {
                id: "s".to_owned(),
                metrics: vec![contract::Metric::State {
                    key: "k".to_owned(),
                    state_labels: None,
                }],
            }],
        }
        .encode()
        .unwrap()
    }

    #[tokio::test]
    async fn a_cloud_down_at_boot_is_asked_again_until_it_takes_the_manifest() {
        // The quirk this fixes: starting used to fail on the first `Unavailable`, so a boot during
        // an outage lost every reading taken before the cloud came back.
        // A rotated token is waited out here as in the drain: one rule for both.
        for answer in [Outcome::Unavailable, Outcome::Credential] {
            // Declines twice, so taking it at all means it was asked a third time.
            let cloud = Fake::always(Outcome::Committed)
                .declaring(vec![answer, answer], Outcome::Committed);
            declare_forever(&cloud, &manifest(), cadence(), 1)
                .await
                .expect("the third ask is taken");
        }
    }

    #[tokio::test]
    async fn a_permanent_refusal_ends_the_run_rather_than_looping() {
        // A manifest the cloud will never take is not an outage: retrying fills the buffer and
        // then starts dropping readings, which is worse than stopping loudly.
        let cloud = Fake::always(Outcome::Committed).declaring(Vec::new(), Outcome::Rejected(422));
        assert_eq!(
            declare_forever(&cloud, &manifest(), cadence(), 1).await,
            Err(Declined::Answer(Outcome::Rejected(422)))
        );
    }
}
