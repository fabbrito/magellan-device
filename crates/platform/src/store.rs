//! Blobs that survive a power cut.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;

/// Named blobs that survive a power cut. Flat: a name is a file name, never a path.
///
/// A power cut mid-write may leave a blob torn, so whoever reads one checks it: the reader of a
/// half-written blob is the device at its next boot, with nobody to ask what was meant.
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

/// A store in one directory: a file a name.
///
/// No staged write and rename: the one writer never overwrites, so a torn file is only ever a new
/// one, and its reader discards it. The directory is synced after a write, or a power cut can
/// forget the file was ever made.
#[derive(Debug)]
pub struct Dir {
    path: PathBuf,
}

impl Dir {
    /// The store at `path`, which must already exist: the service manager makes it, and one made
    /// here would hide a unit that points somewhere unwritable.
    ///
    /// # Errors
    ///
    /// If `path` is not a readable directory.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        fs::read_dir(&path)?;
        Ok(Self { path })
    }

    fn sync_directory(&self) -> io::Result<()> {
        File::open(&self.path)?.sync_all()
    }
}

/// A name that stays inside the directory and is not hidden.
pub(crate) fn plain(name: &str) -> io::Result<&str> {
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
        let mut file = File::create(self.path.join(plain(name)?))?;
        file.write_all(bytes)?;
        file.sync_all()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot_id;

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
    fn a_name_that_leaves_the_directory_is_refused() {
        let scratch = Scratch::new();
        let store = Dir::open(scratch.0.clone()).unwrap();
        for name in ["", "../escape", "a/b", ".hidden"] {
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
