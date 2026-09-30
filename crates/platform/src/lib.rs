//! Layer 7 — the platform seam. The runtime never names an OS; a platform supplies the clock, the
//! boot id and the store, the OS-specific bits nothing else can. One platform is built — Linux on 32-bit ARM
//! — and the seam stays anyway, because the runtime is written against a shape rather than an OS.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use contract::limits::{BOOT_ID_LENGTH_MAX, BOOT_ID_LENGTH_MIN};

/// Wall-clock and monotonic time, in the units the contract and the heartbeat use.
pub trait Clock {
    /// Milliseconds since the Unix epoch, UTC — the `Reading::ts` the runtime stamps.
    fn now_ms(&self) -> u64;

    /// Seconds since boot. The heartbeat's `boot_id` is what makes a reset explainable, not uptime.
    fn uptime_seconds(&self) -> u64;
}

/// The Linux clock: wall time from the OS, uptime from a monotonic instant taken at construction.
///
/// Uptime is therefore since this process started, not since the board powered on. For a device
/// whose binary starts at boot they are the same number, and where they differ the process restart
/// is the event worth seeing — it comes paired with a fresh `boot_id`.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    started: Instant,
}

impl SystemClock {
    /// Start the uptime count. Call once, where the program starts.
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        // A Pi carries no clock of its own until NTP steps it, so this reads 1970 for the first
        // seconds of a boot. Reported as it is: a timestamp the runtime can see is wrong beats one
        // the platform quietly invented.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            })
    }

    fn uptime_seconds(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}

/// Bytes of entropy behind a boot id: sixteen hex digits, inside the contract's 8..=32.
const BOOT_ID_BYTES: usize = 8;

/// A fresh boot id, drawn from the operating system's random source.
///
/// Half of what identifies a batch (ADR 8), so it has to differ between two runs of the same
/// device or the second run's gaps hide behind the first's sequence. Entropy,
/// not a counter: the alternative was a number written to flash, which is the thing that decision
/// removed.
///
/// # Errors
///
/// If the random source cannot be read. There is no fallback on purpose — a boot id the device
/// invented from its clock would collide exactly when the clock has not been set, which is every
/// cold boot without a network.
pub fn boot_id() -> io::Result<String> {
    let mut bytes = [0_u8; BOOT_ID_BYTES];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex::encode(bytes))
}

const _: () = assert!(BOOT_ID_BYTES * 2 >= BOOT_ID_LENGTH_MIN);
const _: () = assert!(BOOT_ID_BYTES * 2 <= BOOT_ID_LENGTH_MAX);

/// Named blobs that survive a power cut. Flat: a name is a file name, never a path.
///
/// A write is whole or absent after a crash, never torn: the reader of a half-written blob would
/// be the device at its next boot, with nobody to ask what was meant.
pub trait Store: Send + Sync {
    /// Every name held, sorted.
    ///
    /// # Errors
    ///
    /// If the store cannot be listed.
    fn list(&self) -> io::Result<Vec<String>>;

    /// The bytes held under `name`.
    ///
    /// # Errors
    ///
    /// If nothing is held under it, or it cannot be read.
    fn read(&self, name: &str) -> io::Result<Vec<u8>>;

    /// Hold `bytes` under `name`, replacing what was there, durably before returning.
    ///
    /// # Errors
    ///
    /// If `name` is not a plain name, or the write did not reach storage.
    fn write(&self, name: &str, bytes: &[u8]) -> io::Result<()>;

    /// Drop `name`. Absent already is not an error.
    ///
    /// # Errors
    ///
    /// If `name` is not a plain name, or it cannot be removed.
    fn remove(&self, name: &str) -> io::Result<()>;
}

/// What a write is staged under before its rename. Never a name [`Store::list`] answers.
const STAGED_PREFIX: &str = ".tmp-";

/// A store in one directory: a file a name, written by the atomic-rename idiom.
///
/// Staged, synced, renamed over, then the directory synced — without the last, a power cut can
/// forget the rename and leave only the staged file. A staged file found at [`Dir::open`] is a
/// write a crash cut short, and its old contents, if any, are still in place.
#[derive(Debug)]
pub struct Dir {
    path: PathBuf,
}

impl Dir {
    /// The store at `path`, which must already exist: the service manager makes it, and one made
    /// here would hide a unit that points somewhere unwritable. Clears writes a crash cut short.
    ///
    /// # Errors
    ///
    /// If `path` is not a directory, or a staged file cannot be cleared.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(STAGED_PREFIX)
            {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(Self { path })
    }

    fn sync_directory(&self) -> io::Result<()> {
        File::open(&self.path)?.sync_all()
    }
}

/// A name that stays inside the directory and is not a staged write's.
fn plain(name: &str) -> io::Result<&str> {
    let well_formed = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if well_formed {
        Ok(name)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not a plain name"),
        ))
    }
}

impl Store for Dir {
    fn list(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.path)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if plain(&name).is_ok() {
                names.push(name);
            }
        }
        names.sort_unstable();
        Ok(names)
    }

    fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        fs::read(self.path.join(plain(name)?))
    }

    fn write(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.path.join(plain(name)?);
        let staged = self.path.join(format!("{STAGED_PREFIX}{name}"));
        let mut file = File::create(&staged)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&staged, &path)?;
        self.sync_directory()
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        // No directory sync: a removal a power cut forgets is a batch sent twice, which the cloud
        // absorbs, and a sync per commit is flash wear bought for nothing.
        match fs::remove_file(self.path.join(plain(name)?)) {
            Err(why) if why.kind() != io::ErrorKind::NotFound => Err(why),
            _ => Ok(()),
        }
    }
}

/// Fakes for tests beside other seams (ADR 5).
#[cfg(feature = "fake")]
pub mod fake {
    use std::collections::BTreeMap;
    use std::io;
    use std::sync::{Mutex, PoisonError};

    use super::{Clock, Store, plain};

    /// A clock stopped at one instant, so a test is about that instant and not about when it ran.
    #[derive(Debug, Clone, Copy)]
    pub struct Stopped {
        pub now_ms: u64,
        pub uptime_seconds: u64,
    }

    impl Stopped {
        /// Stopped at `now_ms`, a second after boot.
        #[must_use]
        pub const fn at(now_ms: u64) -> Self {
            Self {
                now_ms,
                uptime_seconds: 1,
            }
        }
    }

    impl Clock for Stopped {
        fn now_ms(&self) -> u64 {
            self.now_ms
        }

        fn uptime_seconds(&self) -> u64 {
            self.uptime_seconds
        }
    }

    /// A store in RAM. Shared by reference between two runtimes, it is a reboot: what one held,
    /// the next finds.
    #[derive(Debug, Default)]
    pub struct Memory {
        blobs: Mutex<BTreeMap<String, Vec<u8>>>,
        /// Every write fails while set: a card gone read-only.
        pub failing: std::sync::atomic::AtomicBool,
    }

    impl Memory {
        fn blobs(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
            self.blobs.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn fails(&self) -> io::Result<()> {
            if self.failing.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(io::Error::other("the fake store is failing"));
            }
            Ok(())
        }
    }

    impl Store for Memory {
        fn list(&self) -> io::Result<Vec<String>> {
            Ok(self.blobs().keys().cloned().collect())
        }

        fn read(&self, name: &str) -> io::Result<Vec<u8>> {
            self.blobs()
                .get(name)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }

        fn write(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
            self.fails()?;
            self.blobs().insert(plain(name)?.to_owned(), bytes.to_vec());
            Ok(())
        }

        fn remove(&self, name: &str) -> io::Result<()> {
            self.fails()?;
            self.blobs().remove(plain(name)?);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2020-01-01T00:00:00Z in milliseconds. A real clock is past it; the same instant expressed
    /// in seconds is not, so this one bound catches both a dead clock and a units slip.
    const A_DATE_ALREADY_PAST: u64 = 1_577_836_800_000;

    #[test]
    fn the_wall_clock_reads_milliseconds_past_a_date_already_gone() {
        assert!(SystemClock::new().now_ms() > A_DATE_ALREADY_PAST);
    }

    #[test]
    fn a_boot_id_is_one_the_contract_accepts() {
        // Checked against the contract rather than against a length: this value goes on the wire
        // as half of what identifies a batch, so the cloud's rule is the one that matters.
        let batch = contract::Batch {
            manifest_hash: "0".repeat(64),
            boot_id: boot_id().expect("the random source is readable"),
            seq: "1".to_owned(),
            readings: vec![contract::Reading {
                source: "source_1".to_owned(),
                ts: 1_758_326_400_000,
                values: std::collections::BTreeMap::from([("power_w".to_owned(), 1_i64)]),
            }],
        };
        assert_eq!(batch.validate(), Ok(()));
    }

    #[test]
    fn two_boots_do_not_share_an_id() {
        // The whole of what ADR 8 rests on. A repeat here aliases two boots in gap detection, and
        // a loss in one hides behind the other's sequence.
        let ids: std::collections::BTreeSet<String> =
            (0..64).map(|_| boot_id().expect("readable")).collect();
        assert_eq!(ids.len(), 64, "a boot id repeated");
    }

    #[test]
    fn uptime_starts_at_zero_and_never_goes_back() {
        let clock = SystemClock::new();
        assert_eq!(clock.uptime_seconds(), 0);
        let first = clock.uptime_seconds();
        let second = clock.uptime_seconds();
        assert!(second >= first);
    }

    /// A directory of its own under the system's temporary one, gone when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("magellan-store-{}", boot_id().unwrap()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_write_reads_back_and_a_removal_is_gone() {
        let scratch = Scratch::new();
        let store = Dir::open(scratch.0.clone()).unwrap();
        store.write("b.json", b"two").unwrap();
        store.write("a.json", b"one").unwrap();
        store.write("a.json", b"uno").unwrap();
        assert_eq!(
            store.list().unwrap(),
            ["a.json", "b.json"],
            "sorted, one per name"
        );
        assert_eq!(store.read("a.json").unwrap(), b"uno");
        store.remove("a.json").unwrap();
        store.remove("a.json").unwrap();
        assert_eq!(store.list().unwrap(), ["b.json"]);
    }

    #[test]
    fn a_write_cut_short_is_invisible_and_cleared_at_open() {
        // A power cut between the staged write and its rename leaves only the staged file.
        let scratch = Scratch::new();
        fs::write(scratch.0.join("a.json"), b"old").unwrap();
        fs::write(scratch.0.join(".tmp-a.json"), b"torn").unwrap();
        let store = Dir::open(scratch.0.clone()).unwrap();
        assert_eq!(store.list().unwrap(), ["a.json"]);
        assert_eq!(
            store.read("a.json").unwrap(),
            b"old",
            "the old contents survive"
        );
        assert!(!scratch.0.join(".tmp-a.json").exists());
    }

    #[test]
    fn a_name_that_leaves_the_directory_is_refused() {
        let scratch = Scratch::new();
        let store = Dir::open(scratch.0.clone()).unwrap();
        for name in ["", "../escape", "a/b", ".tmp-a", ".hidden"] {
            let refused = store.write(name, b"x").unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::InvalidInput, "{name:?}");
        }
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn a_missing_directory_is_not_made() {
        // The service manager makes it; one made here hides a unit pointing somewhere else.
        let scratch = Scratch::new();
        assert!(Dir::open(scratch.0.join("absent")).is_err());
    }
}
