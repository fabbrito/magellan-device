//! Fakes for tests beside other seams (ADR 5).

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::store::plain;
use crate::{Clock, Store};

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
    pub failing: AtomicBool,
}

impl Memory {
    fn blobs(&self) -> MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
        self.blobs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn fails(&self) -> io::Result<()> {
        if self.failing.load(Ordering::Relaxed) {
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
