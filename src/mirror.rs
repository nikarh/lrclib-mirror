//! The database being served, and its replacement by newer dumps without a restart.
//!
//! Data directory layout:
//! - `dumps/<dump>.sqlite3`: the database (the newest one is served), `.partial` while it downloads
//! - `search-index/`: the Tantivy index of track names, carried over from one dump to the next (track ids only grow,
//!   so only the tracks a newer dump adds are indexed)

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use server::AppState;
use server::search_index::{IndexerConfig, IndexerHandle, SearchIndex, spawn_indexer};

use crate::config::Config;
use crate::dump::{self, DumpError};

const PARTIAL: &str = ".partial";

#[derive(Debug, thiserror::Error)]
pub enum MirrorError {
    #[error(transparent)]
    Dump(#[from] DumpError),
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("opening {0}: {1}")]
    Open(PathBuf, String),
    #[error("{0} is out, but only {1} GB are free (at least {2} GB needed)")]
    NoSpace(String, u64, u64),
    #[error("no database in {0} and updates are off")]
    NoDatabase(PathBuf),
}

/// An open dump: what the API handlers use.
pub struct Db {
    pub state: Arc<AppState>,
    name: String,
    path: PathBuf,
    indexer: Mutex<Option<IndexerHandle>>,
}

impl Db {
    /// Stops the search indexer, which releases the index for the next dump's. Searches keep working.
    fn stop_indexer(&self) {
        let handle = self.indexer.lock().ok().and_then(|mut i| i.take());
        if let Some(handle) = handle {
            handle.shutdown();
        }
    }
}

pub struct Mirror {
    config: Config,
    client: reqwest::Client,
    current: ArcSwapOption<Db>,
    /// Serializes database changes (updates and shutdown).
    lock: tokio::sync::Mutex<()>,
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
        })
    }

    pub fn current(&self) -> Option<Arc<Db>> {
        self.current.load_full()
    }

    fn dumps_dir(&self) -> PathBuf {
        self.config.data_dir.join("dumps")
    }

    fn index_dir(&self) -> PathBuf {
        self.config.data_dir.join("search-index")
    }

    /// Opens the newest dump in the data directory, if there is one, and removes leftovers (interrupted downloads,
    /// older dumps).
    pub async fn start(&self) -> Result<(), MirrorError> {
        let _guard = self.lock.lock().await;
        let dir = self.dumps_dir();
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| MirrorError::Io(dir.clone(), e))?;

        let mut dumps = Vec::new();
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
                tracing::info!(file = name, "removing an interrupted download");
                remove(&entry.path()).await;
            } else if let Some(key) = name
                .strip_suffix(".sqlite3")
                .map(|n| format!("{n}.sqlite3.gz"))
                && dump::is_dump_key(&key)
            {
                dumps.push(name);
            }
        }
        dumps.sort();
        let Some(newest) = dumps.pop() else {
            if self.config.update_interval.is_none() {
                return Err(MirrorError::NoDatabase(dir));
            }
            tracing::info!(
                "no database yet; the API answers 503 until the first dump is downloaded"
            );
            return Ok(());
        };
        for old in dumps {
            tracing::info!(file = old, "removing an older dump");
            remove_db(&dir.join(old)).await;
        }

        let db = self.open(newest, None).await?;
        self.current.store(Some(Arc::new(db)));
        Ok(())
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

    /// Downloads the newest dump when it isn't the one served, and switches to it.
    async fn update(&self) -> Result<(), MirrorError> {
        let Some(key) = dump::latest(&self.client, &self.config.listing_url).await? else {
            tracing::warn!(listing = self.config.listing_url, "no dumps listed");
            return Ok(());
        };
        let name = dump::db_file_name(&key).to_owned();
        let served = self.current().map(|db| db.name.clone());
        if served.as_deref() >= Some(name.as_str()) {
            tracing::debug!(dump = key, "the newest dump is being served");
            return Ok(());
        }

        let dir = self.dumps_dir();
        let free = free_bytes(&dir).map_err(|e| MirrorError::Io(dir.clone(), e))?;
        if free < self.config.min_free_bytes {
            return Err(MirrorError::NoSpace(
                key,
                free >> 30,
                self.config.min_free_bytes >> 30,
            ));
        }

        let path = dir.join(&name);
        let partial = dir.join(format!("{name}{PARTIAL}"));
        tracing::info!(dump = key, "downloading a new dump");
        let url = format!("{}/{key}", self.config.dump_url);
        if let Err(e) = dump::download(&self.client, &url, &partial).await {
            remove(&partial).await;
            return Err(e.into());
        }
        let checked = partial.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || dump::check(&checked))
            .await
            .map_err(|e| MirrorError::Open(partial.clone(), e.to_string()))?
        {
            remove_db(&partial).await;
            return Err(e.into());
        }
        tokio::fs::rename(&partial, &path)
            .await
            .map_err(|e| MirrorError::Io(path.clone(), e))?;
        // SQLite's side files of the check follow the database.
        for side in ["-wal", "-shm"] {
            let _ = tokio::fs::rename(side_file(&partial, side), side_file(&path, side)).await;
        }

        let _guard = self.lock.lock().await;
        let previous = self.current();
        let db = match self.open(name, previous.clone()).await {
            Ok(db) => db,
            Err(e) => {
                remove_db(&path).await;
                return Err(e);
            }
        };
        self.current.store(Some(Arc::new(db)));
        tracing::info!(dump = key, "serving the new dump");
        if let Some(previous) = previous {
            tokio::spawn(retire(previous));
        }
        Ok(())
    }

    /// Opens a dump and its search index. With a `previous` dump, its indexer is stopped first (the index allows one
    /// writer) and this returns only once the index has caught up, so searches keep working until the switch.
    async fn open(&self, name: String, previous: Option<Arc<Db>>) -> Result<Db, MirrorError> {
        let path = self.dumps_dir().join(&name);
        let index_dir = self.index_dir();
        tracing::info!(file = %path.display(), "opening the database");
        let replacing = previous.is_some();
        let opened = path.clone();
        let (state, indexer, index) = tokio::task::spawn_blocking(move || {
            let pool = server::db::init_db(&opened).map_err(|e| format!("{e:#}"))?;
            if let Some(previous) = previous {
                previous.stop_indexer();
            }
            let (index, indexer) = match SearchIndex::open_or_create(&index_dir) {
                Ok(index) => {
                    let index = Arc::new(index);
                    let indexer = spawn_indexer(Arc::clone(&index), pool.clone(), IndexerConfig::default());
                    (Some(index), Some(indexer))
                }
                Err(e) => {
                    tracing::error!(error = %e, dir = %index_dir.display(), "no search index; search is unavailable");
                    (None, None)
                }
            };
            Ok((server::build_app_state(pool, index.clone()), indexer, index))
        })
        .await
        .map_err(|e| MirrorError::Open(path.clone(), e.to_string()))?
        .map_err(|e: String| MirrorError::Open(path.clone(), e))?;

        // At startup the API serves right away (searches fail until the index has caught up); a replacement waits.
        if replacing
            && let Some(index) = index
            && !index.is_ready()
        {
            tracing::info!("waiting for the search index to catch up with the database");
            while !index.is_ready() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        Ok(Db {
            state,
            name,
            path,
            indexer: Mutex::new(indexer),
        })
    }

    /// Stops the search indexer with a final commit.
    pub async fn shutdown(&self) {
        let _guard = self.lock.lock().await;
        if let Some(db) = self.current.swap(None) {
            tokio::task::spawn_blocking(move || db.stop_indexer())
                .await
                .ok();
        }
    }
}

/// Removes a replaced dump once the requests still using it are done.
async fn retire(db: Arc<Db>) {
    for _ in 0..600 {
        if Arc::strong_count(&db) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let path = db.path.clone();
    drop(db);
    tracing::info!(file = %path.display(), "removing the replaced dump");
    remove_db(&path).await;
}

fn side_file(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

async fn remove(path: &Path) {
    if let Err(e) = tokio::fs::remove_file(path).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(file = %path.display(), error = %e, "can't remove");
    }
}

async fn remove_db(path: &Path) {
    remove(path).await;
    remove(&side_file(path, "-wal")).await;
    remove(&side_file(path, "-shm")).await;
}

fn free_bytes(dir: &Path) -> std::io::Result<u64> {
    let stat = rustix::fs::statvfs(dir)?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}
