//! The bounded buffer of batches awaiting upload: refused, stamped, queued and released under one
//! lock, and written through to the store as it goes.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use contract::{Batch, Encoded, Manifest, Reading, Refusal};
use platform::Store;
use tracing::warn;

/// A bounded, at-least-once queue of batches, oldest first, shared by the poll and the drain.
///
/// Full, it drops the oldest batch rather than growing into the memory and flash the rest of the
/// device needs. The dropped batch already carries its `seq`, so what reaches the cloud has a
/// visible gap where it was — a health signal, never something hidden (ADR 4).
///
/// Written through: a batch reaches the store as it is queued and leaves it as it is released, so
/// a power cut costs nothing queued. RAM holds the same batches, so a send reads no flash; it waits
/// on a write only while a sweep's batch is being stored under the lock.
/// Each batch is a blob named by its place in the queue, which outlives a boot, so a boot resumes
/// the order the last one left. The manifests those batches name are kept beside them: a batch
/// from an earlier boot may name one this boot no longer declares.
///
/// The boot id is drawn once and the counter starts at zero, which together identify a batch
/// (ADR 8). Stamping and queueing share the lock, so `seq` order is queue order.
///
/// A reading the contract would refuse never reaches a batch (ADR 3): the cloud's `4xx` is not how
/// the device finds out.
pub struct Buffer {
    manifest: Manifest,
    encoded: Encoded,
    boot_id: String,
    capacity: NonZeroUsize,
    store: Arc<dyn Store>,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    batches: VecDeque<Held>,
    /// Every manifest a held batch names, and this boot's, by hash.
    manifests: BTreeMap<String, Encoded>,
    /// The place the next batch takes: past every one the store has ever held.
    index: u64,
    seq: u64,
    dropped: u64,
}

/// A batch and its place in the queue, which names its blob.
#[derive(Debug)]
struct Held {
    index: u64,
    batch: Batch,
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

/// What [`Buffer::open`] found in the store — the journal's to report.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Opened {
    /// Batches an earlier boot left, queued ahead of this boot's.
    pub kept: u32,
    /// Blobs dropped, by name, and why.
    pub discarded: Vec<(String, String)>,
}

/// What a blob's name says it holds.
enum Blob {
    Batch(u64),
    Manifest(String),
}

/// Digits in a batch's name: `u64::MAX` is twenty wide, and zero-padding makes the store's sorted
/// names the queue's order.
const INDEX_DIGITS: usize = 20;
const MANIFEST_PREFIX: &str = "manifest-";
const SUFFIX: &str = ".json";

fn batch_name(index: u64) -> String {
    format!("{index:0INDEX_DIGITS$}{SUFFIX}")
}

fn manifest_name(hash: &str) -> String {
    format!("{MANIFEST_PREFIX}{hash}{SUFFIX}")
}

/// `None` for a name this buffer did not write, which it leaves alone.
fn blob_of(name: &str) -> Option<Blob> {
    let stem = name.strip_suffix(SUFFIX)?;
    if let Some(hash) = stem.strip_prefix(MANIFEST_PREFIX) {
        return Some(Blob::Manifest(hash.to_owned()));
    }
    let digits = stem.len() == INDEX_DIGITS && stem.chars().all(|c| c.is_ascii_digit());
    if !digits {
        return None;
    }
    stem.parse().ok().map(Blob::Batch)
}

/// A batch's JSON. Never fails on one serde can build: strings, integers and string-keyed maps.
fn serialized(batch: &Batch) -> Vec<u8> {
    let bytes = serde_json::to_vec(batch);
    assert!(bytes.is_ok(), "a batch failed to serialize");
    bytes.unwrap_or_default()
}

/// A batch as an earlier boot wrote it, refused as the cloud would refuse it.
fn load_batch(store: &dyn Store, name: &str) -> Result<Batch, String> {
    let bytes = store.read(name).map_err(|why| why.to_string())?;
    let batch: Batch = serde_json::from_slice(&bytes).map_err(|why| why.to_string())?;
    batch.validate().map_err(|why| why.to_string())?;
    Ok(batch)
}

/// A manifest as an earlier boot wrote it, refused unless its bytes hash to its name.
fn load_manifest(store: &dyn Store, name: &str, hash: &str) -> Result<Encoded, String> {
    let bytes = store.read(name).map_err(|why| why.to_string())?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|why| why.to_string())?;
    let encoded = manifest.encode().map_err(|why| why.to_string())?;
    if encoded.hash() != hash || encoded.bytes() != bytes.as_slice() {
        return Err("its bytes do not hash to its name".to_owned());
    }
    Ok(encoded)
}

impl Buffer {
    /// A buffer of at most `capacity` batches of readings `manifest` declares, stamped with the
    /// hash of `encoded`, in the boot `boot_id` names — queued behind whatever `store` kept.
    ///
    /// Non-zero by the type: a buffer that can hold nothing drops every reading the moment it is
    /// made, and would look like a working device doing it.
    ///
    /// A blob that will not load is dropped and reported, never fatal: one torn by a power cut
    /// must not keep the device from booting.
    ///
    /// # Errors
    ///
    /// If the store cannot be listed.
    pub fn open(
        capacity: NonZeroUsize,
        manifest: Manifest,
        encoded: Encoded,
        boot_id: String,
        store: Arc<dyn Store>,
    ) -> io::Result<(Self, Opened)> {
        let mut opened = Opened::default();
        let mut batches = Vec::new();
        let mut manifests = BTreeMap::new();
        for name in store.list()? {
            let loaded = match blob_of(&name) {
                None => continue,
                Some(Blob::Batch(index)) => load_batch(store.as_ref(), &name)
                    .map(|batch| batches.push(Held { index, batch })),
                Some(Blob::Manifest(hash)) => load_manifest(store.as_ref(), &name, &hash)
                    .map(|encoded| drop(manifests.insert(hash, encoded))),
            };
            if let Err(why) = loaded {
                discard(store.as_ref(), &name);
                opened.discarded.push((name, why));
            }
        }
        batches.sort_unstable_by_key(|held| held.index);
        let index = batches
            .last()
            .map_or(0, |held| held.index.saturating_add(1));
        let buffer = Self {
            manifest,
            encoded,
            boot_id,
            capacity,
            store,
            state: Mutex::new(State {
                batches: VecDeque::with_capacity(capacity.get()),
                manifests,
                index,
                seq: 0,
                dropped: 0,
            }),
        };
        buffer.resume(batches, &mut opened);
        Ok((buffer, opened))
    }

    /// Queue what an earlier boot left, oldest dropped past the bound, and keep this boot's
    /// manifest beside them.
    fn resume(&self, batches: Vec<Held>, opened: &mut Opened) {
        let mut state = self.state();
        let hash = self.encoded.hash().to_owned();
        // Written only when absent: the store never overwrites, so a blob is torn only if new.
        if !state.manifests.contains_key(&hash)
            && let Err(why) = self
                .store
                .write(&manifest_name(&hash), self.encoded.bytes())
        {
            warn!(%why, "manifest not stored; batches read under it may not outlive this boot");
        }
        state.manifests.insert(hash, self.encoded.clone());
        let past = batches.len().saturating_sub(self.capacity.get());
        for (n, held) in batches.into_iter().enumerate() {
            if n < past {
                opened
                    .discarded
                    .push((batch_name(held.index), "past the bound".to_owned()));
                self.forget(&mut state, &held);
            } else {
                state.batches.push_back(held);
            }
        }
        // A manifest no batch names is one no batch will ask to be declared.
        let named: Vec<String> = state.manifests.keys().cloned().collect();
        for hash in named {
            self.forget_manifest_unless_named(&mut state, &hash);
        }
        opened.kept = depth_of(&state.batches);
    }

    /// Refuse what the contract would, stamp the rest as the next batch, store and append it,
    /// dropping the oldest when full.
    ///
    /// The `seq` is spent whether or not the batch is ever delivered — a number spent on a batch
    /// later dropped is exactly the gap that shows the loss.
    ///
    /// A batch the store would not take is still queued, and journalled: it is lost to a power
    /// cut, not to an outage, and refusing it would lose it to both.
    ///
    /// # Panics
    ///
    /// When the batch the buffer stamped breaks the contract. The readings in it have passed, so
    /// what broke is the envelope the runtime built itself: a programmer error, not an operating
    /// one.
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
        let seq = state.seq.to_string();
        let batch = Batch {
            manifest_hash: self.encoded.hash().to_owned(),
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
        let index = state.index;
        state.index = state.index.saturating_add(1);
        if let Err(why) = self.store.write(&batch_name(index), &serialized(&batch)) {
            warn!(seq, %why, "batch not stored; a power cut loses it");
        }
        let displaced = if state.batches.len() >= self.capacity.get() {
            state.dropped = state.dropped.saturating_add(1);
            state.batches.pop_front().map(|held| {
                let seq = held.batch.seq.clone();
                self.forget(&mut state, &held);
                seq
            })
        } else {
            None
        };
        state.batches.push_back(Held { index, batch });
        Enqueued {
            refused,
            queued: Some(Queued {
                seq,
                depth: depth_of(&state.batches),
                displaced,
                dropped: state.dropped,
            }),
        }
    }

    /// A copy of the oldest batch, so no lock is held across the send.
    #[must_use]
    pub fn front(&self) -> Option<Batch> {
        self.state().batches.front().map(|held| held.batch.clone())
    }

    /// Drop `sent` if it is still the oldest batch. Returns whether it was.
    ///
    /// While a request is in flight the poll may overflow the buffer and push `sent` out, putting
    /// another at the front. Popping blindly would drop a batch that was never sent — a reading
    /// lost with no `seq` gap to show for it.
    pub fn release(&self, sent: &Batch) -> bool {
        let mut state = self.state();
        let still_ours = state.batches.front().is_some_and(|front| {
            front.batch.boot_id == sent.boot_id && front.batch.seq == sent.seq
        });
        if still_ours && let Some(held) = state.batches.pop_front() {
            self.forget(&mut state, &held);
        }
        still_ours
    }

    /// Batches still queued.
    #[must_use]
    pub fn depth(&self) -> u32 {
        depth_of(&self.state().batches)
    }

    /// The manifests batches from earlier boots name, other than this boot's, oldest first.
    #[must_use]
    pub fn earlier_manifests(&self) -> Vec<Encoded> {
        let state = self.state();
        let mut earlier: Vec<Encoded> = Vec::new();
        for held in &state.batches {
            let hash = held.batch.manifest_hash.as_str();
            let seen = hash == self.encoded.hash() || earlier.iter().any(|e| e.hash() == hash);
            if !seen && let Some(encoded) = state.manifests.get(hash) {
                earlier.push(encoded.clone());
            }
        }
        earlier
    }

    /// The manifest the oldest batch names, if the buffer holds it.
    #[must_use]
    pub fn front_manifest(&self) -> Option<Encoded> {
        let state = self.state();
        let hash = &state.batches.front()?.batch.manifest_hash;
        state.manifests.get(hash).cloned()
    }

    /// Drop every batch naming `hash`: the cloud will never take the manifest they were read
    /// under. Returns how many went; their `seq`s are the gap.
    pub fn drop_named(&self, hash: &str) -> u32 {
        let mut state = self.state();
        let (named, kept): (VecDeque<Held>, VecDeque<Held>) = std::mem::take(&mut state.batches)
            .into_iter()
            .partition(|held| held.batch.manifest_hash == hash);
        state.batches = kept;
        for held in &named {
            self.forget(&mut state, held);
        }
        depth_of(&named)
    }

    /// Unstore a batch already out of the queue, and its manifest with it once nothing names it.
    fn forget(&self, state: &mut State, held: &Held) {
        discard(self.store.as_ref(), &batch_name(held.index));
        self.forget_manifest_unless_named(state, &held.batch.manifest_hash);
    }

    fn forget_manifest_unless_named(&self, state: &mut State, hash: &str) {
        let named = hash == self.encoded.hash()
            || state
                .batches
                .iter()
                .any(|held| held.batch.manifest_hash == hash);
        if !named {
            state.manifests.remove(hash);
            discard(self.store.as_ref(), &manifest_name(hash));
        }
    }

    /// Every operation leaves the queue whole, so a lock poisoned by a panic elsewhere still
    /// guards a usable queue. Refusing it would stop the drain for good and drop every sweep
    /// silently.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A blob left behind costs a duplicate at the next boot, which the cloud absorbs: journalled,
/// never fatal.
fn discard(store: &dyn Store, name: &str) {
    if let Err(why) = store.remove(name) {
        warn!(name, %why, "blob not removed; the next boot may send it again");
    }
}

/// Saturating: the contract bounds what it will accept, and a depth past `u32` means the bound was
/// never applied. Reporting the ceiling beats wrapping to nothing.
fn depth_of(batches: &VecDeque<Held>) -> u32 {
    u32::try_from(batches.len()).unwrap_or(u32::MAX)
}

/// One source, `inverter`, declaring `power_w` — what the tests beside the buffer fill it with.
#[cfg(test)]
impl Buffer {
    pub(crate) fn fixture(capacity: usize) -> Self {
        Self::fixture_on(capacity, Arc::new(platform::fake::Memory::default()))
    }

    /// As [`Buffer::fixture`], over `store`: a second one over the same store is a reboot.
    pub(crate) fn fixture_on(capacity: usize, store: Arc<dyn Store>) -> Self {
        let (manifest, encoded) = Self::fixture_manifest();
        Self::open(
            NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN),
            manifest,
            encoded,
            "0123456789abcdef".to_owned(),
            store,
        )
        .expect("the fake store lists")
        .0
    }

    /// The manifest a fixture declares, and its encoding.
    pub(crate) fn fixture_manifest() -> (Manifest, Encoded) {
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
        let encoded = manifest.encode().expect("within the contract");
        (manifest, encoded)
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

    use std::sync::atomic::Ordering;

    use platform::fake::Memory;

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
            .map(|held| held.batch.seq.clone())
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
            .map(|held| held.batch.boot_id.clone())
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

    /// The fixture's manifest in another zone: another hash, as a boot after an update declares.
    fn reopened_under_another_manifest(store: &Arc<Memory>) -> Buffer {
        let manifest = Manifest {
            tz: "America/Sao_Paulo".to_owned(),
            ..Buffer::fixture_manifest().0
        };
        let encoded = manifest.encode().expect("within the contract");
        Buffer::open(
            NonZeroUsize::MIN.saturating_add(7),
            manifest,
            encoded,
            "fedcba9876543210".to_owned(),
            store.clone(),
        )
        .expect("lists")
        .0
    }

    fn names(store: &Memory) -> Vec<String> {
        store.list().expect("lists")
    }

    #[test]
    fn a_reboot_resumes_the_queue_where_the_last_left_it() {
        // The power cut this exists for: nothing queued is lost, and order survives the boot.
        let store = Arc::new(Memory::default());
        let before = Buffer::fixture_on(8, store.clone());
        before.fill(3);
        let sent = before.front().expect("three queued");
        assert!(before.release(&sent));
        drop(before);

        let after = Buffer::fixture_on(8, store);
        assert_eq!(seqs(&after), ["1", "2"]);
        after.fill(1);
        assert_eq!(
            seqs(&after),
            ["1", "2", "0"],
            "this boot's queue behind the last's"
        );
        let indices: Vec<u64> = after
            .state()
            .batches
            .iter()
            .map(|held| held.index)
            .collect();
        assert_eq!(indices, [1, 2, 3], "the index continues past the stored");
    }

    #[test]
    fn what_leaves_the_queue_leaves_the_store() {
        let store = Arc::new(Memory::default());
        let buffer = Buffer::fixture_on(2, store.clone());
        buffer.fill(3);
        let sent = buffer.front().expect("queued");
        assert!(buffer.release(&sent));
        let batches: Vec<String> = names(&store)
            .into_iter()
            .filter(|name| !name.starts_with(MANIFEST_PREFIX))
            .collect();
        assert_eq!(batches, [batch_name(2)], "released and displaced both gone");
    }

    #[test]
    fn a_blob_that_will_not_load_is_dropped_and_named() {
        // A torn write on a failing card must not keep the device from booting.
        let store = Arc::new(Memory::default());
        Buffer::fixture_on(8, store.clone()).fill(1);
        store.write(&batch_name(7), b"{ torn").expect("writes");
        store
            .write("notes.txt", b"not the buffer's")
            .expect("writes");
        let (manifest, encoded) = Buffer::fixture_manifest();
        let (_, opened) = Buffer::open(
            NonZeroUsize::MIN.saturating_add(7),
            manifest,
            encoded,
            "fedcba9876543210".to_owned(),
            store.clone(),
        )
        .expect("lists");
        assert_eq!(opened.kept, 1);
        let discarded: Vec<&str> = opened.discarded.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(discarded, [batch_name(7)]);
        assert!(!names(&store).contains(&batch_name(7)));
        assert!(
            names(&store).contains(&"notes.txt".to_owned()),
            "not its to drop"
        );
    }

    #[test]
    fn a_store_past_a_smaller_bound_keeps_the_newest() {
        let store = Arc::new(Memory::default());
        Buffer::fixture_on(8, store.clone()).fill(5);
        let smaller = Buffer::fixture_on(2, store.clone());
        assert_eq!(seqs(&smaller), ["3", "4"]);
        assert!(!names(&store).contains(&batch_name(0)));
    }

    #[test]
    fn an_earlier_manifest_is_kept_while_a_batch_names_it() {
        // A batch drains after its own manifest, so that manifest must outlive the boot.
        let store = Arc::new(Memory::default());
        let before = Buffer::fixture_on(8, store.clone());
        before.fill(1);
        let earlier = before.encoded.clone();
        drop(before);

        let after = reopened_under_another_manifest(&store);
        assert!(after.state().manifests.contains_key(earlier.hash()));
        assert!(names(&store).contains(&manifest_name(earlier.hash())));

        let sent = after.front().expect("the earlier boot's");
        assert!(after.release(&sent));
        assert!(!after.state().manifests.contains_key(earlier.hash()));
        assert!(!names(&store).contains(&manifest_name(earlier.hash())));
        assert!(
            names(&store).contains(&manifest_name(after.encoded.hash())),
            "this boot's"
        );
    }

    #[test]
    fn an_earlier_manifest_is_named_once_and_its_batches_can_be_dropped_whole() {
        let store = Arc::new(Memory::default());
        let before = Buffer::fixture_on(8, store.clone());
        before.fill(2);
        let earlier = before.encoded.clone();
        drop(before);
        let after = reopened_under_another_manifest(&store);
        after.fill(1);
        assert_eq!(
            after.earlier_manifests(),
            std::slice::from_ref(&earlier),
            "once, and never this boot's"
        );
        assert_eq!(after.drop_named(earlier.hash()), 2);
        assert_eq!(seqs(&after), ["0"], "this boot's batch stays");
        assert_eq!(after.earlier_manifests(), Vec::<Encoded>::new());
        assert!(!names(&store).contains(&manifest_name(earlier.hash())));
    }

    #[test]
    fn a_manifest_no_batch_names_is_dropped_at_open() {
        let store = Arc::new(Memory::default());
        let earlier = Buffer::fixture_on(8, store.clone()).encoded;
        reopened_under_another_manifest(&store);
        assert!(!names(&store).contains(&manifest_name(earlier.hash())));
    }

    #[test]
    fn a_manifest_whose_bytes_are_not_its_name_is_dropped() {
        let store = Arc::new(Memory::default());
        let hash = "0".repeat(64);
        let (manifest, encoded) = Buffer::fixture_manifest();
        store
            .write(&manifest_name(&hash), encoded.bytes())
            .expect("writes");
        let opened = Buffer::open(
            NonZeroUsize::MIN,
            manifest,
            encoded,
            "fedcba9876543210".to_owned(),
            store,
        )
        .expect("lists")
        .1;
        assert_eq!(opened.discarded.len(), 1);
    }

    #[test]
    fn a_store_that_will_not_write_still_queues() {
        // Lost to a power cut, not to an outage: refusing it would lose it to both.
        let store = Arc::new(Memory::default());
        let buffer = Buffer::fixture_on(4, store.clone());
        store.failing.store(true, Ordering::Relaxed);
        buffer.fill(2);
        assert_eq!(seqs(&buffer), ["0", "1"]);
        let sent = buffer.front().expect("queued");
        assert!(buffer.release(&sent));
        assert_eq!(buffer.depth(), 1);
    }
}
