//! The store being served, and its replacement by one imported from a newer dump, without a restart.
//!
//! Data directory layout:
//! - `stores/<dump>/`: a store (the newest finished one is served); `<dump>.partial/` while it is imported
//! - `dumps/`: dumps to import when updates are off (`lrclib-db-dump-<date>.sqlite3` or `.sqlite3.gz`)

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arc_swap::ArcSwapOption;

use crate::config::Config;
use crate::dump::{self, DumpError};
use crate::store::{self, ImportError, ImportOptions, Store, StoreError};

const PARTIAL: &str = ".partial";

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
    #[error("the import stopped: {0}")]
    Join(String),
}

pub struct Mirror {
    config: Config,
    client: reqwest::Client,
    current: ArcSwapOption<Store>,
    /// Serializes store changes (updates and shutdown).
    lock: tokio::sync::Mutex<()>,
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

    /// Opens the newest store, and removes leftovers (interrupted imports, older stores, stores this version can't
    /// read). With updates off, imports a dump from `dumps/` that is newer than the store.
    pub async fn start(&self) -> Result<(), MirrorError> {
        let guard = self.lock.lock().await;
        let dir = self.stores_dir();
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| MirrorError::Io(dir.clone(), e))?;

        let mut stores = Vec::new();
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .map_err(|e| MirrorError::Io(dir.clone(), e))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| MirrorError::Io(dir.clone(), e))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(PARTIAL) {
                tracing::info!(store = name, "removing an interrupted import");
                remove_dir(&entry.path()).await;
            } else if dump::is_dump_key(&format!("{name}.sqlite3.gz")) {
                stores.push(name);
            }
        }
        stores.sort();
        while let Some(newest) = stores.pop() {
            let path = dir.join(&newest);
            match open(path.clone()).await {
                Ok(store) => {
                    tracing::info!(store = newest, tracks = store.meta.tracks, "serving");
                    self.current.store(Some(Arc::new(store)));
                    break;
                }
                Err(e) => {
                    tracing::warn!(store = newest, error = %e, "removing a store that can't be opened");
                    remove_dir(&path).await;
                }
            }
        }
        for old in stores {
            tracing::info!(store = old, "removing an older store");
            remove_dir(&dir.join(old)).await;
        }

        if self.config.update_interval.is_none() {
            let local = self.local_dump().await;
            match local {
                Some((path, name)) if self.served_name().as_deref() < Some(name.as_str()) => {
                    drop(guard);
                    self.import_local(&path, &name).await?;
                }
                _ if self.current().is_none() => {
                    return Err(MirrorError::NoData(dir, self.dumps_dir()));
                }
                _ => {}
            }
        } else if self.current().is_none() {
            tracing::info!("no store yet; the API answers 503 until the first dump is imported");
        }
        Ok(())
    }

    /// The newest dump in `dumps/`, and the name of its store.
    async fn local_dump(&self) -> Option<(PathBuf, String)> {
        let dir = self.dumps_dir();
        let mut entries = tokio::fs::read_dir(&dir).await.ok()?;
        let mut best: Option<(PathBuf, String)> = None;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let file = entry.file_name().to_string_lossy().into_owned();
            let key = match file.strip_suffix(".gz") {
                Some(_) => file.clone(),
                None => format!("{file}.gz"),
            };
            if !dump::is_dump_key(&key) {
                continue;
            }
            let name = dump::store_name(&key).to_owned();
            if best.as_ref().is_none_or(|(_, b)| *b < name) {
                best = Some((entry.path(), name));
            }
        }
        best
    }

    async fn import_local(&self, path: &Path, name: &str) -> Result<(), MirrorError> {
        tracing::info!(dump = %path.display(), "importing");
        let file = std::fs::File::open(path).map_err(|e| MirrorError::Io(path.to_owned(), e))?;
        let gz = path.extension().is_some_and(|e| e == "gz");
        self.import_and_switch(name, move || -> Box<dyn std::io::Read + Send> {
            let file = std::io::BufReader::with_capacity(1 << 20, file);
            if gz {
                Box::new(dump::gunzip(file))
            } else {
                Box::new(file)
            }
        })
        .await
    }

    /// Checks for a newer dump every `update_interval`, forever.
    pub async fn run_updates(self: Arc<Self>) {
        let Some(interval) = self.config.update_interval else {
            return;
        };
        loop {
            if let Err(e) = self.update().await {
                tracing::error!(error = %e, "dump update failed");
            }
            tokio::time::sleep(interval).await;
        }
    }

    /// Imports the newest dump when it isn't the one served, and switches to it.
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
        let free = free_bytes(&dir).map_err(|e| MirrorError::Io(dir.clone(), e))?;
        if free < self.config.min_free_bytes {
            return Err(MirrorError::NoSpace(
                key,
                free >> 30,
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

    /// Imports a dump into `stores/<name>`, then serves it and removes the store it replaces.
    async fn import_and_switch<F>(&self, name: &str, input: F) -> Result<(), MirrorError>
    where
        F: FnOnce() -> Box<dyn std::io::Read + Send> + Send + 'static,
    {
        let path = self.stores_dir().join(name);
        let partial = self.stores_dir().join(format!("{name}{PARTIAL}"));
        remove_dir(&partial).await;
        let options = ImportOptions {
            threads: self.config.import_threads,
            memory: self.config.import_memory_bytes,
            ..ImportOptions::default()
        };
        let stop = Arc::clone(&self.stop);
        let target = partial.clone();
        let imported =
            tokio::task::spawn_blocking(move || store::import(input(), &target, &options, &stop))
                .await
                .map_err(|e| MirrorError::Join(e.to_string()))?;
        let stats = match imported {
            Ok(stats) => stats,
            Err(e) => {
                remove_dir(&partial).await;
                return Err(e.into());
            }
        };
        tracing::info!(
            tracks = stats.tracks,
            minutes = (stats.seconds_pass1 + stats.seconds_pass2) / 60,
            "imported"
        );
        tokio::fs::rename(&partial, &path)
            .await
            .map_err(|e| MirrorError::Io(path.clone(), e))?;

        let _guard = self.lock.lock().await;
        let store = match open(path.clone()).await {
            Ok(store) => store,
            Err(e) => {
                remove_dir(&path).await;
                return Err(e);
            }
        };
        let previous = self.current.swap(Some(Arc::new(store)));
        tracing::info!(store = name, "serving the new store");
        if let Some(previous) = previous {
            tokio::spawn(retire(previous));
        }
        Ok(())
    }

    /// Stops an import in progress.
    pub async fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _guard = self.lock.lock().await;
    }
}

async fn open(path: PathBuf) -> Result<Store, MirrorError> {
    tokio::task::spawn_blocking(move || Store::open(&path))
        .await
        .map_err(|e| MirrorError::Join(e.to_string()))?
        .map_err(MirrorError::Store)
}

/// Removes a replaced store once the requests still using it are done.
async fn retire(store: Arc<Store>) {
    for _ in 0..600 {
        if Arc::strong_count(&store) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let path = store.dir().to_owned();
    drop(store);
    tracing::info!(store = %path.display(), "removing the replaced store");
    remove_dir(&path).await;
}

async fn remove_dir(path: &Path) {
    if let Err(e) = tokio::fs::remove_dir_all(path).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(dir = %path.display(), error = %e, "can't remove");
    }
}

fn free_bytes(dir: &Path) -> std::io::Result<u64> {
    let stat = rustix::fs::statvfs(dir)?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}
