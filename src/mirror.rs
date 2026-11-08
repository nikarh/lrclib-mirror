//! The store being served, and its replacement by one imported from a newer dump, without a restart.
//!
//! Data directory layout:
//! - `stores/<dump>/`: a store (the newest finished one is served);
//! - `stores/<dump>.partial/`: a store being imported. An import interrupted after it read the dump continues from
//!   its last checkpoint, without the dump; one interrupted while reading the dump starts over (the dump is streamed,
//!   never stored);
//! - `stores/<name>.removing/`: a store being removed. Nothing is removed in place, so a crash never leaves part of a
//!   store under a name that would be served or continued;
//! - `dumps/`: dumps to import when updates are off (`lrclib-db-dump-<date>.sqlite3` or `.sqlite3.gz`).
//!
//! The process can stop at any point. A store is served only whole (finished, every file as written), and removed
//! only when it is shown to be damaged ([`StoreError::is_damage`]); one built by another version is kept until a new
//! one replaces it. The data directory is locked by the process using it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwapOption;

use crate::config::Config;
use crate::dump::{self, DumpError};
use crate::store::file::{DirLock, sync_dir};
use crate::store::{self, ImportError, ImportOptions, Store, StoreError, default_memory};

const PARTIAL: &str = ".partial";
const REMOVING: &str = ".removing";
/// After a failed update, the next try comes this soon (or at the update interval, when shorter).
const RETRY: Duration = Duration::from_hours(1);

#[derive(Debug, thiserror::Error)]
pub enum MirrorError {
    #[error(transparent)]
    Dump(#[from] DumpError),
    #[error(transparent)]
    Import(#[from] ImportError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("{0} is out, but only {1} GB are free (at least {2} GB needed)")]
    NoSpace(String, u64, u64),
    #[error("no store in {0}, no dump to import in {1}, and updates are off")]
    NoData(PathBuf, PathBuf),
    #[error("{0} is in use by another process")]
    Locked(PathBuf),
    #[error("the import stopped: {0}")]
    Join(String),
}

pub struct Mirror {
    config: Config,
    client: reqwest::Client,
    current: ArcSwapOption<Store>,
    /// Serializes store changes (updates and shutdown).
    lock: tokio::sync::Mutex<()>,
    /// The data directory, for this process only (from `start`).
    dir_lock: Mutex<Option<DirLock>>,
    stop: Arc<AtomicBool>,
}

impl Mirror {
    pub fn new(config: Config) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                "lrclib-mirror/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/nikarh/lrclib-mirror)"
            ))
            .connect_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            config,
            client,
            current: ArcSwapOption::empty(),
            lock: tokio::sync::Mutex::new(()),
            dir_lock: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn current(&self) -> Option<Arc<Store>> {
        self.current.load_full()
    }

    fn stores_dir(&self) -> PathBuf {
        self.config.data_dir.join("stores")
    }

    fn dumps_dir(&self) -> PathBuf {
        self.config.data_dir.join("dumps")
    }

    fn served_name(&self) -> Option<String> {
        self.current().and_then(|s| {
            s.dir()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
    }

    /// Opens the newest store this version can serve, and tidies up: what was being removed, older stores, imports
    /// of dumps not newer than the one served. With updates off, imports a dump from `dumps/` that is newer than the
    /// store (or continues its interrupted import).
    pub async fn start(&self) -> Result<(), MirrorError> {
        let guard = self.lock.lock().await;
        let data = self.config.data_dir.clone();
        let dir = self.stores_dir();
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| MirrorError::Io(dir.clone(), e))?;
        // One process at a time: two would remove and replace each other's stores and imports.
        let lock = DirLock::try_lock(&data)
            .map_err(|e| MirrorError::Io(data.clone(), e))?
            .ok_or_else(|| MirrorError::Locked(data.clone()))?;
        if let Ok(mut held) = self.dir_lock.lock() {
            *held = Some(lock);
        }

        let mut stores = Vec::new();
        let mut partials = Vec::new();
        for (name, path) in list_dir(&dir).await? {
            if name.ends_with(REMOVING) {
                tracing::info!(dir = name, "removing what was being removed");
                remove_dir(&path).await;
            } else if let Some(store) = name.strip_suffix(PARTIAL) {
                partials.push(store.to_owned());
            } else if is_store_name(&name) {
                stores.push(name);
            }
        }
        // An import that finished but wasn't moved yet.
        for partial in &partials {
            if store::finished_store(&dir.join(format!("{partial}{PARTIAL}")))
                && !stores.contains(partial)
            {
                self.publish(partial).await?;
                stores.push(partial.clone());
            }
        }
        stores.sort();
        let mut kept = Vec::new();
        while let Some(newest) = stores.pop() {
            let path = dir.join(&newest);
            match open(path.clone()).await {
                Ok(store) => {
                    tracing::info!(store = newest, tracks = store.meta.tracks, "serving");
                    self.current.store(Some(Arc::new(store)));
                    break;
                }
                Err(MirrorError::Store(e)) if e.is_damage() => {
                    tracing::warn!(store = newest, error = %e, "this store is damaged; removing it");
                    self.discard(&path).await;
                }
                Err(MirrorError::Store(e)) if e.is_other_version() => {
                    // Whole, but not for this version: kept (for the version that built it) until a new store
                    // replaces it.
                    tracing::warn!(store = newest, error = %e, "can't serve this store (kept until replaced)");
                    kept.push(newest);
                }
                // Nothing known about the store (permissions, memory, the disk): not removed, nothing served.
                Err(e) => return Err(e),
            }
        }
        let served = self.served_name();
        if served.is_some() {
            for old in stores.into_iter().chain(kept) {
                tracing::info!(store = old, "removing a store replaced by a newer one");
                self.discard(&dir.join(old)).await;
            }
        }
        for partial in partials {
            if served.as_deref() >= Some(partial.as_str()) {
                tracing::info!(
                    store = partial,
                    "removing the import of a dump already served"
                );
                self.discard(&dir.join(format!("{partial}{PARTIAL}"))).await;
            }
        }

        if self.config.update_interval.is_none() {
            match self.local_dump().await {
                Some((path, name)) if served.as_deref() < Some(name.as_str()) => {
                    drop(guard);
                    self.import_local(&path, &name).await?;
                }
                _ if served.is_none() => {
                    return Err(MirrorError::NoData(dir, self.dumps_dir()));
                }
                _ => {}
            }
        } else if served.is_none() {
            tracing::info!("no store yet; the API answers 503 until the first dump is imported");
        }
        Ok(())
    }

    /// The newest dump in `dumps/`, and the name of its store.
    async fn local_dump(&self) -> Option<(PathBuf, String)> {
        let mut best: Option<(PathBuf, String)> = None;
        for (file, path) in list_dir(&self.dumps_dir()).await.ok()? {
            // Dump names are lowercase (`is_dump_key`).
            let key = if Path::new(&file).extension().is_some_and(|e| e == "gz") {
                file.clone()
            } else {
                format!("{file}.gz")
            };
            if !dump::is_dump_key(&key) {
                continue;
            }
            let name = dump::store_name(&key).to_owned();
            if best.as_ref().is_none_or(|(_, b)| *b < name) {
                best = Some((path, name));
            }
        }
        best
    }

    async fn import_local(&self, path: &Path, name: &str) -> Result<(), MirrorError> {
        tracing::info!(dump = %path.display(), "importing");
        let file = std::fs::File::open(path).map_err(|e| MirrorError::Io(path.to_owned(), e))?;
        let gz = path.extension().is_some_and(|e| e == "gz");
        self.import_and_switch(name, move || -> Box<dyn std::io::Read + Send> {
            if gz {
                Box::new(dump::gunzip(file))
            } else {
                Box::new(dump::read_ahead(file))
            }
        })
        .await
    }

    /// Checks for a newer dump every `update_interval` (sooner after a failure), forever.
    pub async fn run_updates(self: Arc<Self>) {
        let Some(interval) = self.config.update_interval else {
            return;
        };
        loop {
            let wait = match self.update().await {
                Ok(()) => interval,
                Err(e) => {
                    tracing::error!(error = %e, "dump update failed");
                    interval.min(RETRY)
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// Imports the newest dump when it isn't the one served (or continues its interrupted import), and switches to
    /// it.
    async fn update(&self) -> Result<(), MirrorError> {
        let Some(key) = dump::latest(&self.client, &self.config.listing_url).await? else {
            tracing::warn!(listing = self.config.listing_url, "no dumps listed");
            return Ok(());
        };
        let name = dump::store_name(&key).to_owned();
        if self.served_name().as_deref() >= Some(name.as_str()) {
            tracing::debug!(dump = key, "the newest dump is being served");
            return Ok(());
        }
        let dir = self.stores_dir();
        let partial = dir.join(format!("{name}{PARTIAL}"));
        // Imports of other dumps are superseded.
        for (other, path) in list_dir(&dir).await? {
            if other.ends_with(PARTIAL) && other != format!("{name}{PARTIAL}") {
                tracing::info!(store = other, "removing the import of an older dump");
                self.discard(&path).await;
            }
        }
        if store::dump_read(&partial) {
            tracing::info!(dump = key, "continuing the interrupted import of this dump");
            return self
                .import_and_switch(&name, || -> Box<dyn std::io::Read + Send> {
                    Box::new(std::io::empty())
                })
                .await;
        }

        // What an interrupted import of this dump left is replaced: it counts as free space.
        let kept = dir_size(partial.clone()).await;
        let free = free_bytes(&dir).map_err(|e| MirrorError::Io(dir.clone(), e))?;
        if free.saturating_add(kept) < self.config.min_free_bytes {
            return Err(MirrorError::NoSpace(
                key,
                free.saturating_add(kept) >> 30,
                self.config.min_free_bytes >> 30,
            ));
        }
        tracing::info!(dump = key, "downloading and importing a new dump");
        let url = format!("{}/{key}", self.config.dump_url);
        let download = dump::download(&self.client, &url).await?;
        self.import_and_switch(&name, move || -> Box<dyn std::io::Read + Send> {
            Box::new(dump::gunzip(download))
        })
        .await
    }

    /// Imports a dump into `stores/<name>.partial` (continuing an interrupted import), then serves it and removes
    /// the store it replaces. An import stopped by a shutdown is kept, to be continued; a failed one is removed.
    async fn import_and_switch<F>(&self, name: &str, input: F) -> Result<(), MirrorError>
    where
        F: FnOnce() -> Box<dyn std::io::Read + Send> + Send + 'static,
    {
        let partial = self.stores_dir().join(format!("{name}{PARTIAL}"));
        let options = ImportOptions::new(
            self.config.import_threads,
            self.config
                .import_memory_bytes
                .unwrap_or_else(default_memory),
        );
        let stop = Arc::clone(&self.stop);
        let target = partial.clone();
        let imported = tokio::task::spawn_blocking(move || {
            let input = (!store::dump_read(&target)).then(input);
            store::import(input, &target, &options, &stop)
        })
        .await
        .map_err(|e| MirrorError::Join(e.to_string()))?;
        let stats = match imported {
            Ok(stats) => stats,
            Err(ImportError::Stopped) => return Err(ImportError::Stopped.into()),
            Err(e) => {
                self.discard(&partial).await;
                return Err(e.into());
            }
        };
        tracing::info!(
            tracks = stats.tracks,
            minutes = (stats.seconds_pass1 + stats.seconds_pass2) / 60,
            "imported"
        );
        self.publish(name).await?;

        let _guard = self.lock.lock().await;
        let path = self.stores_dir().join(name);
        let store = match open(path.clone()).await {
            Ok(store) => store,
            Err(e) => {
                if let MirrorError::Store(e) = &e
                    && e.is_damage()
                {
                    self.discard(&path).await;
                }
                return Err(e);
            }
        };
        let previous = self.current.swap(Some(Arc::new(store)));
        tracing::info!(store = name, "serving the new store");
        if let Some(previous) = previous {
            tokio::spawn(retire(previous, self.stores_dir()));
        }
        Ok(())
    }

    /// Moves a finished import to its store's name.
    async fn publish(&self, name: &str) -> Result<(), MirrorError> {
        let dir = self.stores_dir();
        let path = dir.join(name);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            self.discard(&path).await;
        }
        tokio::fs::rename(dir.join(format!("{name}{PARTIAL}")), &path)
            .await
            .map_err(|e| MirrorError::Io(path.clone(), e))?;
        sync_dir(&dir).map_err(|e| MirrorError::Io(dir.clone(), e))
    }

    /// Removes a store or an import: first renamed, so that what is left after a crash is never taken for it.
    async fn discard(&self, path: &Path) {
        discard(path, &self.stores_dir()).await;
    }

    /// Stops an import in progress (it continues at the next start).
    pub async fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _guard = self.lock.lock().await;
    }
}

fn is_store_name(name: &str) -> bool {
    dump::is_dump_key(&format!("{name}.sqlite3.gz"))
}

async fn list_dir(dir: &Path) -> Result<Vec<(String, PathBuf)>, MirrorError> {
    let io = |e| MirrorError::Io(dir.to_owned(), e);
    let mut entries = tokio::fs::read_dir(dir).await.map_err(io)?;
    let mut out = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(io)? {
        out.push((
            entry.file_name().to_string_lossy().into_owned(),
            entry.path(),
        ));
    }
    Ok(out)
}

async fn open(path: PathBuf) -> Result<Store, MirrorError> {
    tokio::task::spawn_blocking(move || Store::open(&path))
        .await
        .map_err(|e| MirrorError::Join(e.to_string()))?
        .map_err(MirrorError::Store)
}

/// Removes a replaced store once the requests still using it are done.
async fn retire(store: Arc<Store>, stores: PathBuf) {
    for _ in 0..600 {
        if Arc::strong_count(&store) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let path = store.dir().to_owned();
    drop(store);
    tracing::info!(store = %path.display(), "removing the replaced store");
    discard(&path, &stores).await;
}

async fn discard(path: &Path, stores: &Path) {
    let Some(name) = path.file_name() else {
        return;
    };
    let mut removing = name.to_owned();
    removing.push(REMOVING);
    let removing = stores.join(removing);
    match tokio::fs::rename(path, &removing).await {
        Ok(()) => {
            let _ = sync_dir(stores);
            remove_dir(&removing).await;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(dir = %path.display(), error = %e, "can't remove"),
    }
}

async fn remove_dir(path: &Path) {
    if let Err(e) = tokio::fs::remove_dir_all(path).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(dir = %path.display(), error = %e, "can't remove");
    }
}

/// The size of the files under `dir` (0 when it doesn't exist).
async fn dir_size(dir: PathBuf) -> u64 {
    fn walk(dir: &Path) -> u64 {
        std::fs::read_dir(dir).map_or(0, |entries| {
            entries
                .flatten()
                .map(|e| match e.metadata() {
                    Ok(m) if m.is_dir() => walk(&e.path()),
                    Ok(m) => m.len(),
                    Err(_) => 0,
                })
                .sum()
        })
    }
    tokio::task::spawn_blocking(move || walk(&dir))
        .await
        .unwrap_or(0)
}

fn free_bytes(dir: &Path) -> std::io::Result<u64> {
    let stat = rustix::fs::statvfs(dir)?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}
