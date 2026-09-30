//! One run of the device: declare, poll, buffer, drain, and stop — the whole of Layer 5 behind one
//! call. The binary chooses what satisfies each seam; everything else is assembled here.

use std::fmt;
use std::io;
use std::sync::Arc;

use contract::Refusal;
use driver::Source;
use platform::{Clock, Store};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::backoff::jitter_seed;
use crate::device::{Polled, Polling, manifest_of};
use crate::drain::{declare_all, drain_forever};
use crate::heartbeat::{Beating, Heard};
use crate::{Buffer, Cloud, Config, Declined};

/// What satisfies each seam the runtime is written against, chosen where the program starts
/// (ADR 1).
pub struct Wiring {
    /// The drivers, one per `[[source]]`, in the order they are polled.
    pub sources: Vec<Box<dyn Source>>,
    pub cloud: Arc<dyn Cloud>,
    pub clock: Arc<dyn Clock + Send + Sync>,
    /// Where the buffer outlives a power cut.
    pub store: Arc<dyn Store>,
    /// Drawn once per boot (ADR 8).
    pub boot_id: String,
    /// What the heartbeat says is running.
    pub firmware: String,
}

/// Why a run ended before it was told to stop. Each is permanent: asking again cannot fix it, so
/// the binary exits without a restart and a person acts.
#[derive(Debug)]
pub enum RunError {
    /// The manifest the sources declare breaks the contract. The cloud would reject it.
    Manifest(Refusal),
    /// The cloud will not take the manifest: rejected, or it hashed other bytes than were sent.
    Declined(Declined),
    /// The store cannot be listed, so what an earlier boot left cannot be found.
    Store(io::Error),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(refusal) => write!(f, "the manifest breaks the contract: {refusal}"),
            Self::Declined(declined) => {
                write!(f, "the cloud will not take the manifest: {declined:?}")
            }
            Self::Store(why) => write!(f, "the buffer's store cannot be listed: {why}"),
        }
    }
}

impl std::error::Error for RunError {}

/// Run until `stop`, or until the manifest can never be taken.
///
/// Polling starts before the manifest is declared: a cloud that is down at boot is the same as one
/// that goes down later, and the readings must not wait on it. The drain starts once the cloud
/// takes every manifest the buffer's batches name. A stop leaves what is queued in the store for
/// the next boot.
///
/// # Errors
///
/// [`RunError`] when asking again cannot help. An outage is never one: the batches stay in the
/// buffer and the declare retries.
pub async fn run(config: Config, wiring: Wiring, stop: CancellationToken) -> Result<(), RunError> {
    let Wiring {
        sources,
        cloud,
        clock,
        store,
        boot_id,
        firmware,
    } = wiring;
    let manifest = manifest_of(&config.zone, &sources);
    // The name a batch carries, computed rather than asked for: the cloud may be down at boot, and
    // the device knows its own manifest (both sides hash the bytes they handle).
    let encoded = manifest.encode().map_err(RunError::Manifest)?;
    let seed = jitter_seed(&boot_id);
    let cadence = config.cadence;
    let (buffer, opened) = Buffer::open(
        config.buffer,
        manifest,
        encoded.clone(),
        boot_id.clone(),
        store,
    )
    .map_err(RunError::Store)?;
    let buffer = Arc::new(buffer);
    for (name, why) in &opened.discarded {
        warn!(name, why, "stored blob dropped");
    }
    if opened.kept > 0 {
        info!(kept = opened.kept, "batches from an earlier boot queued");
    }
    let heard = Arc::new(Heard::default());
    let beating = Beating {
        cloud: Arc::clone(&cloud),
        buffer: Arc::clone(&buffer),
        clock: Arc::clone(&clock),
        heard: Arc::clone(&heard),
        boot_id,
        firmware,
        period: cadence.heartbeat,
    };
    // A source is matched to its window by id, which the config holds unique.
    let sources = sources
        .into_iter()
        .map(|source| Polled {
            window: config
                .sources
                .iter()
                .find(|written| written.id == source.id())
                .and_then(|written| written.window),
            source,
        })
        .collect();
    let polling = Polling {
        sources,
        buffer: Arc::clone(&buffer),
        clock,
        cadence,
        heard,
    };

    // Its own token, so a run that ends on a refusal stops the poll it started.
    let done = stop.child_token();
    // Before the declare: a heartbeat names no manifest, and a cloud down at boot is when one
    // matters most.
    let beat = tokio::spawn(beating.run(done.clone()));
    let poll = tokio::spawn(polling.run(done.clone()));
    let declared = tokio::select! {
        declared = declare_all(&buffer, cloud.as_ref(), &encoded, cadence, seed) => declared,
        () = done.cancelled() => Ok(()),
    };
    if let Err(declined) = declared {
        done.cancel();
        let _ = tokio::join!(poll, beat);
        report_unsent(&buffer);
        return Err(RunError::Declined(declined));
    }
    if done.is_cancelled() {
        let _ = tokio::join!(poll, beat);
        report_unsent(&buffer);
        return Ok(());
    }
    info!(hash = encoded.hash(), "manifest accepted");

    let drain = tokio::spawn(drain_forever(
        Arc::clone(&buffer),
        cloud,
        cadence,
        seed,
        done.clone(),
    ));
    done.cancelled().await;
    let _ = tokio::join!(poll, drain, beat);
    report_unsent(&buffer);
    Ok(())
}

/// What the store keeps for the next boot, which sends it first.
fn report_unsent(buffer: &Buffer) {
    let depth = buffer.depth();
    if depth > 0 {
        info!(
            depth,
            "stopping with batches unsent; kept for the next boot"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use contract::{Batch, Encoded, Heartbeat};
    use platform::fake::{Memory, Stopped};
    use tokio::task::JoinHandle;
    use tokio::time::{sleep, timeout};

    use super::*;
    use crate::fake::Fake;
    use crate::{Cadence, Outcome, Token};

    /// Midday in São Paulo, on a slot boundary: the window is open and the first sweep is one
    /// period away.
    fn midday() -> u64 {
        let at: jiff::Timestamp = "2026-09-17T15:00:00Z"
            .parse()
            .unwrap_or(jiff::Timestamp::UNIX_EPOCH);
        u64::try_from(at.as_millisecond()).unwrap_or(0)
    }

    fn config() -> Config {
        Config {
            device_id: "device_1".to_owned(),
            token: Token::fixture("s3cret"),
            endpoint: "http://127.0.0.1".to_owned(),
            request_timeout: Duration::from_secs(20),
            cadence: Cadence {
                sweep: Duration::from_secs(300),
                backoff_first: Duration::from_secs(5),
                backoff_ceiling: Duration::from_secs(60),
                drain_pace: Duration::from_secs(1),
                heartbeat: Duration::from_hours(1),
            },
            buffer: NonZeroUsize::new(8).unwrap_or(NonZeroUsize::MIN),
            buffer_dir: std::path::PathBuf::new(),
            zone: "America/Sao_Paulo".to_owned(),
            sources: Vec::new(),
        }
    }

    fn inverter(id: &str) -> Box<dyn Source> {
        Box::new(driver::fake::Fake::answering(id, "power_w", 1))
    }

    /// A cloud that behaves as `Fake` does, and keeps the `seq` of every batch it committed — what
    /// a real one would have stored.
    struct Storing {
        cloud: Fake,
        committed: Mutex<Vec<String>>,
    }

    impl Storing {
        fn new(cloud: Fake) -> Arc<Self> {
            Arc::new(Self {
                cloud,
                committed: Mutex::new(Vec::new()),
            })
        }

        fn committed(&self) -> Vec<String> {
            self.committed.lock().map(|c| c.clone()).unwrap_or_default()
        }
    }

    #[async_trait]
    impl Cloud for Storing {
        async fn declare(&self, manifest: &Encoded) -> Result<String, Declined> {
            self.cloud.declare(manifest).await
        }

        async fn beat(&self, heartbeat: &Heartbeat) -> Outcome {
            self.cloud.beat(heartbeat).await
        }

        async fn send(&self, batch: &Batch) -> Outcome {
            let outcome = self.cloud.send(batch).await;
            if outcome == Outcome::Committed
                && let Ok(mut committed) = self.committed.lock()
            {
                committed.push(batch.seq.clone());
            }
            outcome
        }
    }

    fn start(
        cloud: Arc<dyn Cloud>,
        sources: Vec<Box<dyn Source>>,
        stop: &CancellationToken,
    ) -> JoinHandle<Result<(), RunError>> {
        start_on(cloud, sources, stop, Arc::new(Memory::default()))
    }

    /// As [`start`], over `store`: what an earlier boot left in it is this run's to send.
    fn start_on(
        cloud: Arc<dyn Cloud>,
        sources: Vec<Box<dyn Source>>,
        stop: &CancellationToken,
        store: Arc<Memory>,
    ) -> JoinHandle<Result<(), RunError>> {
        let wiring = Wiring {
            sources,
            cloud,
            clock: Arc::new(Stopped::at(midday())),
            store,
            boot_id: "0123456789abcdef".to_owned(),
            firmware: "0.1.0-test".to_owned(),
        };
        tokio::spawn(run(config(), wiring, stop.clone()))
    }

    #[tokio::test(start_paused = true)]
    async fn a_sweep_reaches_the_cloud() {
        let cloud = Storing::new(Fake::always(Outcome::Committed));
        let stop = CancellationToken::new();
        let running = start(cloud.clone(), vec![inverter("inverter")], &stop);
        // The first slot, and a drain's look after it.
        sleep(Duration::from_secs(310)).await;
        stop.cancel();
        assert!(matches!(running.await, Ok(Ok(()))));
        assert_eq!(cloud.committed(), ["0"]);
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_lost_while_the_cloud_is_down_at_boot() {
        // The poll does not wait on the declare: three sweeps land while the cloud comes back.
        let cloud = Storing::new(
            Fake::always(Outcome::Committed)
                .declaring(vec![Outcome::Unavailable; 10], Outcome::Committed),
        );
        let stop = CancellationToken::new();
        let running = start(cloud.clone(), vec![inverter("inverter")], &stop);
        sleep(Duration::from_secs(1000)).await;
        stop.cancel();
        assert!(matches!(running.await, Ok(Ok(()))));
        assert_eq!(cloud.committed(), ["0", "1", "2"]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_earlier_boots_batches_go_first_under_their_own_manifest() {
        // The power cut ADR 4 now survives: the last boot's queue drains before this one's.
        let store = Arc::new(Memory::default());
        Buffer::fixture_on(8, store.clone()).fill(2);
        let cloud = Storing::new(Fake::always(Outcome::Committed));
        let stop = CancellationToken::new();
        let running = start_on(
            cloud.clone(),
            vec![inverter("inverter")],
            &stop,
            store.clone(),
        );
        sleep(Duration::from_secs(310)).await;
        stop.cancel();
        assert!(matches!(running.await, Ok(Ok(()))));
        assert_eq!(cloud.committed(), ["0", "1", "0"]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_earlier_manifest_refused_costs_only_its_own_batches() {
        let store = Arc::new(Memory::default());
        Buffer::fixture_on(8, store.clone()).fill(2);
        // The earlier manifest is declared first, and refused; this boot's is taken.
        let cloud = Storing::new(
            Fake::always(Outcome::Committed)
                .declaring(vec![Outcome::Rejected(422)], Outcome::Committed),
        );
        let stop = CancellationToken::new();
        let running = start_on(cloud.clone(), vec![inverter("inverter")], &stop, store);
        sleep(Duration::from_secs(310)).await;
        stop.cancel();
        assert!(matches!(running.await, Ok(Ok(()))));
        assert_eq!(cloud.committed(), ["0"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_keeps_what_the_cloud_did_not_take_for_the_next_boot() {
        let store = Arc::new(Memory::default());
        let cloud = Storing::new(Fake::always(Outcome::Unavailable));
        let stop = CancellationToken::new();
        let running = start_on(cloud, vec![inverter("inverter")], &stop, store.clone());
        sleep(Duration::from_secs(310)).await;
        stop.cancel();
        assert!(matches!(running.await, Ok(Ok(()))));
        let next = Buffer::fixture_on(8, store);
        assert_eq!(next.depth(), 1, "the sweep outlived the stop");
    }

    #[tokio::test]
    async fn a_manifest_the_contract_refuses_ends_the_run() {
        // Two sources with one id: the cloud would reject it, so asking is not worth a request.
        let cloud = Storing::new(Fake::always(Outcome::Committed));
        let stop = CancellationToken::new();
        let running = start(
            cloud,
            vec![inverter("inverter"), inverter("inverter")],
            &stop,
        );
        let ended = timeout(Duration::from_secs(1), running).await;
        assert!(
            matches!(ended, Ok(Ok(Err(RunError::Manifest(_))))),
            "{ended:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_manifest_the_cloud_rejects_ends_the_run_and_its_poll() {
        // Returning at all means the poll it started was stopped: the run waits on it.
        let cloud = Storing::new(
            Fake::always(Outcome::Committed).declaring(Vec::new(), Outcome::Rejected(422)),
        );
        let stop = CancellationToken::new();
        let running = start(cloud, vec![inverter("inverter")], &stop);
        let ended = timeout(Duration::from_secs(1), running).await;
        assert!(
            matches!(
                ended,
                Ok(Ok(Err(RunError::Declined(Declined::Answer(
                    Outcome::Rejected(422)
                )))))
            ),
            "{ended:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_while_the_cloud_is_down_ends_the_run() {
        let cloud = Storing::new(
            Fake::always(Outcome::Committed).declaring(Vec::new(), Outcome::Unavailable),
        );
        let stop = CancellationToken::new();
        let running = start(cloud, vec![inverter("inverter")], &stop);
        sleep(Duration::from_secs(10)).await;
        stop.cancel();
        let ended = timeout(Duration::from_secs(1), running).await;
        assert!(matches!(ended, Ok(Ok(Ok(())))), "{ended:?}");
    }
}
