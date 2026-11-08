//! Files that survive a crash: written whole or not at all, on disk before what depends on them; and a lock so that
//! one process at a time works in a directory.

use std::fs::File;
use std::io::Write as _;
use std::path::Path;

/// Writes `path` whole or not at all: a temporary file, synced, renamed over it, and the directory synced.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let mut f = File::create(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    sync_dir(path.parent().unwrap_or_else(|| Path::new(".")))
}

/// Syncs a directory: the files created, renamed or removed in it are on disk.
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

/// An exclusive lock on a directory, held until dropped (or the process ends).
#[derive(Debug)]
pub struct DirLock(#[allow(dead_code)] File);

impl DirLock {
    /// Locks `dir`; `None` when another process holds it.
    pub fn try_lock(dir: &Path) -> std::io::Result<Option<Self>> {
        let file = File::open(dir)?;
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(Self(file))),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_writes_and_locks() {
        let dir = std::env::temp_dir().join(format!("lrclib-mirror-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert!(!dir.join("x.json.tmp").exists());
        let lock = DirLock::try_lock(&dir).unwrap();
        assert!(lock.is_some());
        assert!(DirLock::try_lock(&dir).unwrap().is_none());
        drop(lock);
        assert!(DirLock::try_lock(&dir).unwrap().is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
