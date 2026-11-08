//! Builds a store from a dump, read as a stream (see [`crate::sqlite`]), in two passes over temporary files:
//!
//! 1. The dump's rows are read once. Tracks are spread over [`BUCKETS`] bucket files by their sort key (so an
//!    artist's tracks share a bucket). Each lyrics row is reduced to what can't be computed back from the rest
//!    (several worker threads) and goes to the bucket of the track whose current lyrics it is; rows whose track hasn't
//!    come yet wait in an extra bucket until the end of the dump. Rows no track points to are dropped.
//! 2. Bucket by bucket (in parallel, written in order): the tracks are sorted, joined with their lyrics, and written
//!    as compressed blocks; the search index is fed.
//!
//! The temporary files take roughly as much space as the finished store; each bucket file is removed once written.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufWriter, Read, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::lyricsfile;
use crate::search;
use crate::sqlite::{self, Value};
use crate::text::{normalize, plain_from_lrc, synced_skeleton};

use super::file;

use super::{
    FORMAT_VERSION, IDS, L_INSTRUMENTAL, L_LF_SHIFT, L_PLAIN_SHIFT, L_SYNCED_SHIFT, L_WORD_SYNC,
    LYRICS, LYRICS_BLOCK_BYTES, LYRICS_DICT, LYRICS_INDEX, META, Meta, Reader, SEARCH, T_ALBUM,
    T_ALBUM_LOWER, T_ARTIST, T_ARTIST_LOWER, T_BROKEN, T_DURATION, T_NAME, T_NAME_LOWER, TRACKS,
    TRACKS_DICT, TRACKS_INDEX, TRACKS_PER_BLOCK, mode, put_bytes, put_varint, sort_key, track_meta,
};

/// Tracks are spread over this many buckets (by the top bits of their sort key) between the two passes.
const BUCKETS: usize = 1024;
const BUCKET_BITS: u32 = BUCKETS.trailing_zeros();
/// The extra bucket of lyrics whose track hasn't been seen yet.
const PENDING: usize = BUCKETS;
const NO_BUCKET: u16 = u16::MAX;
/// A bucket's rows are compressed into the bucket file in frames of about this size.
const LYRICS_FRAME: usize = 128 << 10;
const TRACKS_FRAME: usize = 32 << 10;
/// Buckets used to train the compression dictionaries.
const TRAINING_BUCKETS: usize = 8;
const LYRICS_DICT_BYTES: usize = 112 << 10;
const TRACKS_DICT_BYTES: usize = 32 << 10;
/// Memory a bucket takes while it is built, per byte of its temporary files (zstd-compressed rows, decompressed and
/// decoded).
const MEMORY_PER_TEMP_BYTE: u64 = 6;
/// Lyrics flag of a temporary entry only: LRCLIB's server can't read the row.
const L_BROKEN: u32 = 1 << 8;

#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// Threads reducing lyrics (pass 1) and compressing buckets (pass 2).
    pub threads: usize,
    /// zstd level of the store's blocks.
    pub level: i32,
    /// Memory for the search index writer.
    pub index_memory: usize,
    /// Memory for the buckets being made in pass 2, roughly.
    pub memory: usize,
    /// Pass 2 makes what it wrote durable every this many buckets (of 1024): an interrupted import continues from
    /// there.
    pub checkpoint_every: usize,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self::new(
            std::thread::available_parallelism().map_or(2, std::num::NonZero::get),
            default_memory(),
        )
    }
}

/// What pass 1 takes besides its buffers per bucket (the routing tables, the reading), and the id map of pass 2.
const FIXED_MEMORY: u64 = 400 << 20;
const MIN_MEMORY: u64 = 768 << 20;

impl ImportOptions {
    /// Options for `threads` threads and about `memory` bytes in all.
    #[must_use]
    pub fn new(threads: usize, memory: u64) -> Self {
        let memory = memory.max(MIN_MEMORY);
        let spare = memory - FIXED_MEMORY;
        let index = (spare / 6).clamp(48 << 20, 256 << 20);
        Self {
            threads: threads.max(1),
            level: 9,
            index_memory: usize::try_from(index).unwrap_or(usize::MAX),
            memory: usize::try_from(spare - index).unwrap_or(usize::MAX),
            checkpoint_every: 128,
        }
    }
}

/// The memory an import takes by default: 60% of what the process may use (its container's limit, or the machine's
/// memory), between 768 MB and 4 GB (more makes little difference).
#[must_use]
pub fn default_memory() -> u64 {
    memory_limit().map_or(1 << 30, |limit| (limit / 5 * 3).clamp(MIN_MEMORY, 4 << 30))
}

/// The memory the process may use: the cgroup's limit (a container's), or the machine's memory.
#[must_use]
pub fn memory_limit() -> Option<u64> {
    let read = |path: &str| std::fs::read_to_string(path).ok();
    let cgroup = read("/sys/fs/cgroup/memory.max")
        .or_else(|| read("/sys/fs/cgroup/memory/memory.limit_in_bytes"))
        .and_then(|s| s.trim().parse::<u64>().ok());
    let total = read("/proc/meminfo").and_then(|s| {
        let line = s.lines().find(|l| l.starts_with("MemTotal:"))?;
        Some(line.split_whitespace().nth(1)?.parse::<u64>().ok()? * 1024)
    });
    match (cgroup, total) {
        (Some(c), Some(t)) => Some(c.min(t)),
        (c, t) => c.or(t),
    }
}

/// What an import found and did (informational: missing fields read as 0).
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ImportStats {
    pub pages: u64,
    pub tracks: u64,
    pub lyrics_rows: u64,
    /// Lyrics rows no track points to (not kept).
    pub lyrics_unused: u64,
    /// Tracks pointing to lyrics that aren't in the dump.
    pub lyrics_missing: u64,
    pub plain_derived: u64,
    pub synced_skeleton: u64,
    pub lyricsfile_built: u64,
    pub lyricsfile_head: u64,
    pub lyricsfile_text: u64,
    pub lower_exceptions: u64,
    pub broken_tracks: u64,
    /// Buckets too large for the memory, made in parts.
    pub split_buckets: u64,
    pub max_early_pages: usize,
    pub seconds_pass1: u64,
    pub seconds_pass2: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error(transparent)]
    Sqlite(#[from] sqlite::SqliteError),
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("search index: {0}")]
    Search(String),
    #[error("the dump is not what LRCLIB's dumps look like: {0}")]
    Data(String),
    #[error("compression: {0}")]
    Compress(std::io::Error),
    #[error("stopped")]
    Stopped,
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> ImportError + '_ {
    move |e| ImportError::Io(path.to_owned(), e)
}

/// A track row as the dump has it.
#[derive(Debug, Clone, Default)]
struct TrackRow {
    id: u32,
    name: Option<String>,
    name_lower: Option<String>,
    artist: Option<String>,
    artist_lower: Option<String>,
    album: Option<String>,
    album_lower: Option<String>,
    duration: Option<f64>,
    last_lyrics_id: Option<u32>,
    broken: bool,
}

/// Builds the store `out` from the dump read from `input` (an SQLite database file as a stream). `stop` aborts it.
///
/// An import interrupted (stopped, killed, or the machine losing power) continues where it can when run again into
/// the same `out`: once the dump has been read (`import.json`), `input` isn't needed anymore (it may be `None`), and
/// pass 2 continues from its last checkpoint. One interrupted while reading the dump starts over. A finished store
/// (`meta.json`, written last) is left as it is.
pub fn import<R: Read>(
    input: Option<R>,
    out: &Path,
    options: &ImportOptions,
    stop: &AtomicBool,
) -> Result<ImportStats, ImportError> {
    std::fs::create_dir_all(out).map_err(io(out))?;
    if let Some(meta) = finished(out) {
        return Ok(meta.stats);
    }
    let tmp = out.join("tmp");
    tracing::info!(
        threads = options.threads,
        bucket_memory_mb = options.memory >> 20,
        index_memory_mb = options.index_memory >> 20,
        "importing"
    );
    let mut progress = if let Some(progress) = read_progress(out) {
        tracing::info!(
            buckets_done = progress.checkpoint.as_ref().map_or(0, |c| c.buckets),
            "the dump was read before; continuing"
        );
        progress
    } else {
        let input = input.ok_or_else(|| ImportError::Data("no dump to read".to_owned()))?;
        // Whatever an import interrupted while reading the dump left.
        clear_dir(out)?;
        std::fs::create_dir_all(&tmp).map_err(io(&tmp))?;
        let mut stats = ImportStats::default();
        let started = Instant::now();
        let max_track_id = pass1(input, &tmp, options, stop, &mut stats)?;
        stats.seconds_pass1 = started.elapsed().as_secs();
        tracing::info!(
            tracks = stats.tracks,
            lyrics = stats.lyrics_rows,
            seconds = stats.seconds_pass1,
            "dump read"
        );
        // The buckets on disk before they are what an interrupted import continues from.
        for entry in std::fs::read_dir(&tmp).map_err(io(&tmp))?.flatten() {
            File::open(entry.path())
                .and_then(|f| f.sync_all())
                .map_err(io(&entry.path()))?;
        }
        file::sync_dir(&tmp).map_err(io(&tmp))?;
        let progress = Progress {
            format: FORMAT_VERSION,
            derivations: super::derivations(),
            max_track_id,
            stats,
            dictionaries: false,
            checkpoint: None,
        };
        write_progress(out, &progress)?;
        progress
    };

    let started = Instant::now();
    let meta = pass2(&tmp, out, &mut progress, options, stop)?;
    let mut stats = meta.stats.clone();
    stats.seconds_pass2 += started.elapsed().as_secs();
    let meta = Meta {
        stats: stats.clone(),
        ..meta
    };
    finish(out, &meta)?;
    Ok(stats)
}

/// The store's `meta.json`, when `out` is a finished store.
fn finished(out: &Path) -> Option<Meta> {
    serde_json::from_slice(&std::fs::read(out.join(META)).ok()?).ok()
}

/// Removes what is in `dir` (not `dir` itself).
fn clear_dir(dir: &Path) -> Result<(), ImportError> {
    for entry in std::fs::read_dir(dir).map_err(io(dir))?.flatten() {
        let path = entry.path();
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed.map_err(io(&path))?;
    }
    Ok(())
}

/// Finishes a store: its files listed (`files.json`), then `meta.json`, which marks it finished; then the import's
/// leftovers removed.
fn finish(out: &Path, meta: &Meta) -> Result<(), ImportError> {
    let tmp = out.join("tmp");
    std::fs::remove_dir_all(&tmp).map_err(io(&tmp))?;
    let _ = std::fs::remove_file(out.join(PROGRESS));
    remove_ids_partials(out, None);
    let files = super::list_files(out).map_err(io(out))?;
    let path = out.join(super::FILES);
    let data =
        serde_json::to_vec_pretty(&files).map_err(|e| ImportError::Io(path.clone(), e.into()))?;
    file::write_atomic(&path, &data).map_err(io(&path))?;
    let path = out.join(META);
    let data =
        serde_json::to_vec_pretty(meta).map_err(|e| ImportError::Io(path.clone(), e.into()))?;
    file::write_atomic(&path, &data).map_err(io(&path))
}

/// `import.json`: the dump was read (pass 1 is done), and how far pass 2 got.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Progress {
    format: u32,
    derivations: u64,
    max_track_id: u32,
    /// What pass 1 found.
    stats: ImportStats,
    /// The compression dictionaries are written.
    dictionaries: bool,
    checkpoint: Option<Checkpoint>,
}

/// Pass 2's output files as they were after `buckets` buckets.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Checkpoint {
    buckets: usize,
    tracks: u64,
    tracks_index: u64,
    lyrics: u64,
    lyrics_index: u64,
    ordinal: u32,
    lyrics_ordinal: u32,
    track_blocks: u32,
    lyrics_blocks: u32,
    stats: ImportStats,
}

const PROGRESS: &str = "import.json";
/// The id map as of a checkpoint (by its bucket count): a checkpoint written but not yet recorded in `import.json`
/// doesn't replace the one recorded.
fn ids_partial(buckets: usize) -> String {
    format!("ids.{buckets:04}.partial")
}

/// Removes the id maps of checkpoints but `keep`'s.
fn remove_ids_partials(out: &Path, keep: Option<usize>) {
    let keep = keep.map(ids_partial);
    for entry in std::fs::read_dir(out).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let partial = name.starts_with("ids.") && name.ends_with(".partial");
        if partial && keep.as_deref() != Some(name.as_str()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The progress of an interrupted import into `out` that can continue (by this version).
fn read_progress(out: &Path) -> Option<Progress> {
    let progress: Progress =
        serde_json::from_slice(&std::fs::read(out.join(PROGRESS)).ok()?).ok()?;
    (progress.format == FORMAT_VERSION && progress.derivations == super::derivations())
        .then_some(progress)
}

fn write_progress(out: &Path, progress: &Progress) -> Result<(), ImportError> {
    let path = out.join(PROGRESS);
    let data =
        serde_json::to_vec_pretty(progress).map_err(|e| ImportError::Io(path.clone(), e.into()))?;
    file::write_atomic(&path, &data).map_err(io(&path))
}

/// Whether `out` is a finished store.
#[must_use]
pub fn finished_store(out: &Path) -> bool {
    finished(out).is_some()
}

/// Whether `out` is an import (finished or not): what `import` may continue into.
#[must_use]
pub fn is_import(out: &Path) -> bool {
    out.join(PROGRESS).exists() || out.join("tmp").is_dir() || finished(out).is_some()
}

/// Whether an import into `out` has read the dump: run again, it continues without it.
#[must_use]
pub fn dump_read(out: &Path) -> bool {
    finished(out).is_some() || read_progress(out).is_some()
}

/// The memory of the process (anonymous: without the files it maps), for the progress log; 0 where unknown.
fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            let line = s.lines().find(|l| l.starts_with("RssAnon:"))?;
            line.split_whitespace().nth(1)?.parse::<u64>().ok()
        })
        .map_or(0, |kb| kb >> 10)
}

/// A bucket file being written: rows are buffered and appended as zstd frames, each `[len u32][frame]`.
struct BucketFile {
    path: PathBuf,
    raw: Vec<u8>,
    frame: usize,
}

impl BucketFile {
    fn new(path: PathBuf, frame: usize) -> Self {
        Self {
            path,
            raw: Vec::new(),
            frame,
        }
    }

    fn push(&mut self, record: &[u8]) -> Result<(), ImportError> {
        put_bytes(&mut self.raw, record);
        if self.raw.len() >= self.frame {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), ImportError> {
        if self.raw.is_empty() {
            return Ok(());
        }
        let frame = zstd::bulk::compress(&self.raw, 1).map_err(ImportError::Compress)?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(io(&self.path))?;
        let len =
            u32::try_from(frame.len()).map_err(|_| ImportError::Data("huge row".to_owned()))?;
        f.write_all(&len.to_le_bytes()).map_err(io(&self.path))?;
        f.write_all(&frame).map_err(io(&self.path))?;
        self.raw.clear();
        Ok(())
    }
}

/// Reads the records of a bucket file.
fn read_bucket(
    path: &Path,
    mut each: impl FnMut(&[u8]) -> Result<(), ImportError>,
) -> Result<(), ImportError> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(ImportError::Io(path.to_owned(), e)),
    };
    let corrupt = || ImportError::Data(format!("corrupt temporary file {}", path.display()));
    let mut at = 0;
    while at < data.len() {
        let len = data
            .get(at..at + 4)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or_else(corrupt)?;
        let len = usize::try_from(len).map_err(|_| corrupt())?;
        let frame = data.get(at + 4..at + 4 + len).ok_or_else(corrupt)?;
        at += 4 + len;
        let raw = zstd::decode_all(frame).map_err(ImportError::Compress)?;
        let mut r = Reader::new(&raw);
        while r.at < raw.len() {
            let n = usize::try_from(r.varint().ok_or_else(corrupt)?).map_err(|_| corrupt())?;
            each(r.bytes(n).ok_or_else(corrupt)?)?;
        }
    }
    Ok(())
}

fn bucket_path(tmp: &Path, kind: &str, bucket: usize) -> PathBuf {
    tmp.join(format!("{kind}-{bucket:04}"))
}

fn bucket_of(key: u64) -> usize {
    usize::try_from(key >> (64 - BUCKET_BITS)).unwrap_or(0)
}

/// Where lyrics rows go: the buckets of the tracks pointing to them, by lyrics id.
#[derive(Default)]
struct Wanted {
    first: Vec<u16>,
    more: HashMap<u32, Vec<u16>>,
    /// Lyrics rows already routed, by id: a track pointing to one of them later needs a copy.
    seen: Vec<bool>,
    late: Vec<(u32, u16, u16)>,
}

impl Wanted {
    fn want(&mut self, lyrics_id: u32, bucket: u16) {
        let i = lyrics_id as usize;
        if self.first.len() <= i {
            self.first.resize(i + 1 + (i >> 3), NO_BUCKET);
        }
        let Some(slot) = self.first.get_mut(i) else {
            return;
        };
        if *slot == NO_BUCKET {
            // If its lyrics came already, they wait in the pending bucket.
            *slot = bucket;
            return;
        }
        if *slot == bucket {
            return;
        }
        let first = *slot;
        self.more.entry(lyrics_id).or_default().push(bucket);
        if self.seen.get(i).copied().unwrap_or(false) && first != NO_BUCKET {
            self.late.push((lyrics_id, first, bucket));
        }
    }

    fn get(&self, lyrics_id: u32) -> (u16, &[u16]) {
        let first = self
            .first
            .get(lyrics_id as usize)
            .copied()
            .unwrap_or(NO_BUCKET);
        let more = self.more.get(&lyrics_id).map_or(&[][..], Vec::as_slice);
        (first, more)
    }

    fn saw(&mut self, lyrics_id: u32) {
        let i = lyrics_id as usize;
        if self.seen.len() <= i {
            self.seen.resize(i + 1 + (i >> 3), false);
        }
        if let Some(s) = self.seen.get_mut(i) {
            *s = true;
        }
    }
}

/// Text as LRCLIB's server reads it (rusqlite `Option<String>`); `Err` when it can't.
fn text(v: Option<&Value<'_>>) -> Result<Option<String>, Option<String>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Text(t)) => String::from_utf8(t.to_vec())
            .map(Some)
            .map_err(|e| Some(String::from_utf8_lossy(e.as_bytes()).into_owned())),
        Some(Value::Blob(b)) => Err(Some(String::from_utf8_lossy(b).into_owned())),
        Some(Value::Int(i)) => Err(Some(i.to_string())),
        Some(Value::Real(r)) => Err(Some(r.to_string())),
    }
}

fn opt_text(v: Option<&Value<'_>>, broken: &mut bool) -> Option<String> {
    text(v).unwrap_or_else(|lossy| {
        *broken = true;
        lossy
    })
}

/// A boolean column as LRCLIB's server reads it (`Option<bool>`, `None` as false).
fn flag(v: Option<&Value<'_>>, broken: &mut bool) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Int(i)) => *i != 0,
        Some(_) => {
            *broken = true;
            false
        }
    }
}

#[allow(clippy::cast_precision_loss)] // as SQLite converts
fn real(v: Option<&Value<'_>>, broken: &mut bool) -> Option<f64> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::Real(r)) => Some(*r),
        Some(Value::Int(i)) => Some(*i as f64),
        Some(_) => {
            *broken = true;
            None
        }
    }
}

fn id(rowid: i64) -> Result<u32, ImportError> {
    u32::try_from(rowid).map_err(|_| ImportError::Data(format!("row id {rowid} out of range")))
}

/// An integer (not NULL) serial type.
fn is_int(t: u64) -> bool {
    (1..=6).contains(&t) || t == 8 || t == 9
}

#[derive(Debug, PartialEq, Eq)]
enum Table {
    Tracks,
    Lyrics,
    Other,
}

/// Which table a row is from, by its columns (the first, the rowid alias, is always NULL):
/// - `tracks`: 11 columns; no other table of the dump has 11.
/// - `lyrics`: 10 to 13 columns (the last ones were added with ALTER TABLE), the fourth (`track_id`) an integer, or
///   NULL with the fifth (`has_plain_lyrics`) an integer. `missing_tracks` also has 10 columns, but its fourth and
///   fifth are names: TEXT affinity stores numbers in them as text, so they are never integers.
fn table(types: &[u64]) -> Table {
    let t = |i: usize| types.get(i).copied().unwrap_or(u64::MAX);
    if t(0) != 0 {
        return Table::Other;
    }
    if types.len() == 11 {
        return Table::Tracks;
    }
    if (10..=13).contains(&types.len()) && (is_int(t(3)) || (t(3) == 0 && is_int(t(4)))) {
        return Table::Lyrics;
    }
    Table::Other
}

/// Puts a track row into its bucket, and notes which bucket its lyrics go to. Returns its id.
fn route_track(
    rowid: i64,
    payload: &[u8],
    tracks: &mut [BucketFile],
    wanted: &mut Wanted,
) -> Result<u32, ImportError> {
    let values = sqlite::decode_record(payload)?;
    let track = track_row(rowid, &values)?;
    let key = sort_key(track.artist_lower.as_deref(), track.name_lower.as_deref());
    let bucket = bucket_of(key);
    if let Some(lyrics_id) = track.last_lyrics_id {
        #[allow(clippy::cast_possible_truncation)] // < BUCKETS
        wanted.want(lyrics_id, bucket as u16);
    }
    let mut record = Vec::with_capacity(128);
    encode_track_row(&track, &mut record);
    if let Some(file) = tracks.get_mut(bucket) {
        file.push(&record)?;
    }
    Ok(track.id)
}

/// An error of a scan: the importer's own (from the row callback), or the database's.
fn scan_error(e: sqlite::SqliteError) -> ImportError {
    match e {
        sqlite::SqliteError::Row(e) => match e.downcast::<ImportError>() {
            Ok(e) => *e,
            Err(e) => ImportError::Data(e.to_string()),
        },
        e => e.into(),
    }
}

/// Lyrics rows to reduce: `(lyrics id, bucket, record)`. Sent in batches, which keeps the threads from waiting on
/// each other for every row.
type Job = Vec<(u32, usize, Vec<u8>)>;
const BATCH_ROWS: usize = 64;

/// Threads reducing lyrics rows (`(lyrics id, bucket, record)`) into their buckets. An error is kept in `failed`.
fn spawn_reducers(
    threads: usize,
    lyrics: &Arc<Vec<Mutex<BucketFile>>>,
    counters: &Arc<Counters>,
    failed: &Arc<Mutex<Option<ImportError>>>,
) -> (
    std::sync::mpsc::SyncSender<Job>,
    Vec<std::thread::JoinHandle<()>>,
) {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(threads * 64);
    let rx = Arc::new(Mutex::new(rx));
    let workers = (0..threads.max(1))
        .map(|_| {
            let (rx, lyrics, counters, failed) = (
                Arc::clone(&rx),
                Arc::clone(lyrics),
                Arc::clone(counters),
                Arc::clone(failed),
            );
            std::thread::spawn(move || {
                loop {
                    let job = match rx.lock() {
                        Ok(rx) => rx.recv(),
                        Err(_) => return,
                    };
                    let Ok(batch) = job else {
                        return;
                    };
                    for (lyrics_id, bucket, payload) in batch {
                        if let Err(e) =
                            reduce_and_route(lyrics_id, bucket, &payload, &lyrics, &counters)
                            && let Ok(mut f) = failed.lock()
                        {
                            f.get_or_insert(e);
                        }
                    }
                }
            })
        })
        .collect();
    (tx, workers)
}

fn pass1<R: Read>(
    input: R,
    tmp: &Path,
    options: &ImportOptions,
    stop: &AtomicBool,
    stats: &mut ImportStats,
) -> Result<u32, ImportError> {
    let mut tracks: Vec<BucketFile> = (0..BUCKETS)
        .map(|b| BucketFile::new(bucket_path(tmp, "tracks", b), TRACKS_FRAME))
        .collect();
    let lyrics: Arc<Vec<Mutex<BucketFile>>> = Arc::new(
        (0..=BUCKETS)
            .map(|b| Mutex::new(BucketFile::new(bucket_path(tmp, "lyrics", b), LYRICS_FRAME)))
            .collect(),
    );
    let mut wanted = Wanted::default();
    let counters = Arc::new(Counters::default());
    let mut max_track_id = 0u32;
    let failed: Arc<Mutex<Option<ImportError>>> = Arc::new(Mutex::new(None));

    let (tx, workers) = spawn_reducers(options.threads, &lyrics, &counters, &failed);

    let mut batch: Job = Vec::with_capacity(BATCH_ROWS);
    let spill = tmp.join("spill");
    let mut rows = 0u64;
    let scanned = sqlite::scan(input, &spill, |rowid, payload| {
        rows += 1;
        if rows.is_multiple_of(1_000_000) {
            if stop.load(Ordering::Relaxed) {
                return Err(Box::new(ImportError::Stopped));
            }
            tracing::info!(
                rows,
                tracks = stats.tracks,
                lyrics = stats.lyrics_rows,
                rss_mb = rss_mb(),
                "reading the dump"
            );
        }
        let Some(types) = sqlite::record_types(payload) else {
            return Ok(());
        };
        match table(&types) {
            Table::Tracks => {
                let id = route_track(rowid, payload, &mut tracks, &mut wanted)?;
                stats.tracks += 1;
                max_track_id = max_track_id.max(id);
            }
            Table::Lyrics => {
                let lyrics_id = id(rowid)?;
                stats.lyrics_rows += 1;
                let (first, more) = wanted.get(lyrics_id);
                let more = more.to_vec();
                wanted.saw(lyrics_id);
                let first = if first == NO_BUCKET {
                    PENDING
                } else {
                    usize::from(first)
                };
                // The rare lyrics rows several tracks point to are reduced once per bucket.
                for bucket in std::iter::once(first).chain(more.into_iter().map(usize::from)) {
                    batch.push((lyrics_id, bucket, payload.to_vec()));
                }
                if batch.len() >= BATCH_ROWS
                    && tx
                        .send(std::mem::replace(
                            &mut batch,
                            Vec::with_capacity(BATCH_ROWS),
                        ))
                        .is_err()
                {
                    return Err(Box::new(ImportError::Data("a worker failed".to_owned())));
                }
            }
            Table::Other => {}
        }
        Ok(())
    });
    if !batch.is_empty() {
        let _ = tx.send(batch);
    }
    drop(tx);
    for w in workers {
        let _ = w.join();
    }
    let _ = std::fs::remove_file(&spill);
    let scan_stats = scanned.map_err(scan_error)?;
    if let Some(e) = failed.lock().ok().and_then(|mut f| f.take()) {
        return Err(e);
    }
    stats.pages = scan_stats.pages;
    stats.max_early_pages = scan_stats.max_early_pages;
    counters.add_to(stats);
    if stats.tracks == 0 {
        return Err(ImportError::Data("no tracks".to_owned()));
    }
    for file in &mut tracks {
        file.flush()?;
    }
    for file in lyrics.iter() {
        file.lock()
            .map_err(|_| ImportError::Data("poisoned".to_owned()))?
            .flush()?;
    }

    route_waiting(tmp, &lyrics, &mut wanted, stats)?;
    Ok(max_track_id)
}

/// Routes the lyrics that waited for their track, and copies lyrics to the buckets of tracks that pointed to them
/// after they were routed.
fn route_waiting(
    tmp: &Path,
    lyrics: &[Mutex<BucketFile>],
    wanted: &mut Wanted,
    stats: &mut ImportStats,
) -> Result<(), ImportError> {
    let lock = |b: usize| -> Result<std::sync::MutexGuard<'_, BucketFile>, ImportError> {
        lyrics
            .get(b)
            .ok_or_else(|| ImportError::Data("bucket".to_owned()))?
            .lock()
            .map_err(|_| ImportError::Data("poisoned".to_owned()))
    };
    let pending_path = bucket_path(tmp, "lyrics", PENDING);
    read_bucket(&pending_path, |record| {
        let mut r = Reader::new(record);
        let lyrics_id = r
            .varint()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| ImportError::Data("corrupt temporary file".to_owned()))?;
        let (first, more) = wanted.get(lyrics_id);
        if first == NO_BUCKET {
            stats.lyrics_unused += 1;
            return Ok(());
        }
        for b in std::iter::once(first).chain(more.iter().copied()) {
            lock(usize::from(b))?.push(record)?;
        }
        Ok(())
    })?;
    let _ = std::fs::remove_file(&pending_path);
    let mut late: BTreeMap<u16, Vec<(u32, u16)>> = BTreeMap::new();
    for (lyrics_id, from, to) in std::mem::take(&mut wanted.late) {
        late.entry(from).or_default().push((lyrics_id, to));
    }
    for (from, copies) in late {
        lock(usize::from(from))?.flush()?;
        let mut found = Vec::new();
        read_bucket(&bucket_path(tmp, "lyrics", usize::from(from)), |record| {
            let lyrics_id = Reader::new(record)
                .varint()
                .and_then(|v| u32::try_from(v).ok());
            for (id, to) in &copies {
                if lyrics_id == Some(*id) {
                    found.push((*to, record.to_vec()));
                }
            }
            Ok(())
        })?;
        for (to, record) in found {
            lock(usize::from(to))?.push(&record)?;
        }
    }
    for b in 0..BUCKETS {
        lock(b)?.flush()?;
    }
    Ok(())
}

#[derive(Default)]
struct Counters {
    plain_derived: AtomicU64,
    synced_skeleton: AtomicU64,
    lyricsfile_head: AtomicU64,
    lyricsfile_text: AtomicU64,
}

impl Counters {
    fn add_to(&self, stats: &mut ImportStats) {
        stats.plain_derived += self.plain_derived.load(Ordering::Relaxed);
        stats.synced_skeleton += self.synced_skeleton.load(Ordering::Relaxed);
        stats.lyricsfile_head += self.lyricsfile_head.load(Ordering::Relaxed);
        stats.lyricsfile_text += self.lyricsfile_text.load(Ordering::Relaxed);
    }
}

fn track_row(rowid: i64, v: &[Value<'_>]) -> Result<TrackRow, ImportError> {
    let mut broken = false;
    let mut t = TrackRow {
        id: id(rowid)?,
        name: opt_text(v.get(1), &mut broken),
        name_lower: opt_text(v.get(2), &mut broken),
        artist: opt_text(v.get(3), &mut broken),
        artist_lower: opt_text(v.get(4), &mut broken),
        album: opt_text(v.get(5), &mut broken),
        album_lower: opt_text(v.get(6), &mut broken),
        duration: real(v.get(7), &mut broken),
        last_lyrics_id: match v.get(8) {
            Some(Value::Int(i)) => u32::try_from(*i).ok(),
            _ => None,
        },
        broken: false,
    };
    t.broken = broken;
    Ok(t)
}

fn put_opt(out: &mut Vec<u8>, s: Option<&str>) {
    match s {
        None => out.push(0),
        Some(s) => {
            out.push(1);
            put_bytes(out, s.as_bytes());
        }
    }
}

/// An optional string; `Err` for corrupt input.
fn get_opt(r: &mut Reader<'_>) -> Result<Option<String>, ()> {
    match r.u8().ok_or(())? {
        0 => Ok(None),
        _ => Ok(Some(r.string().ok_or(())?.to_owned())),
    }
}

fn encode_track_row(t: &TrackRow, out: &mut Vec<u8>) {
    put_varint(out, u64::from(t.id));
    put_varint(out, t.last_lyrics_id.map_or(0, |i| u64::from(i) + 1));
    out.push(u8::from(t.broken));
    for s in [
        &t.name,
        &t.name_lower,
        &t.artist,
        &t.artist_lower,
        &t.album,
        &t.album_lower,
    ] {
        put_opt(out, s.as_deref());
    }
    match t.duration {
        None => out.push(0),
        Some(d) => {
            out.push(1);
            out.extend_from_slice(&d.to_le_bytes());
        }
    }
}

fn decode_track_row(b: &[u8]) -> Option<TrackRow> {
    let mut r = Reader::new(b);
    let id = u32::try_from(r.varint()?).ok()?;
    let last = r.varint()?;
    let broken = r.u8()? != 0;
    let mut s: [Option<String>; 6] = Default::default();
    for slot in &mut s {
        *slot = get_opt(&mut r).ok()?;
    }
    let duration = match r.u8()? {
        0 => None,
        _ => Some(f64::from_le_bytes(r.bytes(8)?.try_into().ok()?)),
    };
    let [name, name_lower, artist, artist_lower, album, album_lower] = s;
    Some(TrackRow {
        id,
        name,
        name_lower,
        artist,
        artist_lower,
        album,
        album_lower,
        duration,
        last_lyrics_id: last.checked_sub(1).and_then(|v| u32::try_from(v).ok()),
        broken,
    })
}

/// The lyrics of a row, as LRCLIB's server reads them.
struct LyricsRow {
    plain: Option<String>,
    synced: Option<String>,
    lyricsfile: Option<String>,
    instrumental: bool,
    word_sync: bool,
    broken: bool,
}

fn lyrics_row(v: &[Value<'_>]) -> LyricsRow {
    let mut broken = false;
    let plain = opt_text(v.get(1), &mut broken);
    let synced = opt_text(v.get(2), &mut broken);
    let instrumental = flag(v.get(6), &mut broken);
    let lyricsfile = opt_text(v.get(10), &mut broken);
    let word_sync = flag(v.get(12), &mut broken);
    LyricsRow {
        plain,
        synced,
        lyricsfile,
        instrumental,
        word_sync,
        broken,
    }
}

/// Reduces a lyrics row (pass 1) and appends it to its bucket: `[lyrics id][entry]`.
fn reduce_and_route(
    lyrics_id: u32,
    bucket: usize,
    payload: &[u8],
    buckets: &[Mutex<BucketFile>],
    counters: &Counters,
) -> Result<(), ImportError> {
    let values = sqlite::decode_record(payload)?;
    let row = lyrics_row(&values);
    let mut out = Vec::with_capacity(payload.len() / 2);
    put_varint(&mut out, u64::from(lyrics_id));
    reduce(&row, &mut out, counters);
    buckets
        .get(bucket)
        .ok_or_else(|| ImportError::Data("bucket".to_owned()))?
        .lock()
        .map_err(|_| ImportError::Data("poisoned".to_owned()))?
        .push(&out)
}

/// Encodes a lyrics entry, leaving out what can be computed back. A Lyricsfile is kept as its metadata part when the
/// rest is what LRCLIB builds from the lyrics (pass 2 then checks whether the metadata is the track's).
fn reduce(row: &LyricsRow, out: &mut Vec<u8>, counters: &Counters) {
    let plain = row.plain.as_deref();
    let synced = row.synced.as_deref();
    let plain_mode = match (plain, synced) {
        (Some(p), Some(s)) if plain_from_lrc(s).as_deref() == Some(p) => mode::DERIVED,
        (Some(_), _) => mode::TEXT,
        (None, _) => mode::NULL,
    };
    let skeleton = match (plain_mode, plain, synced) {
        (mode::TEXT, Some(p), Some(s)) => synced_skeleton(s, p),
        _ => None,
    };
    let synced_mode = match (&skeleton, synced) {
        (Some(_), _) => mode::DERIVED,
        (None, Some(_)) => mode::TEXT,
        (None, None) => mode::NULL,
    };
    let head = row.lyricsfile.as_deref().and_then(|lf| {
        let at = lyricsfile::body_start(lf)?;
        let body = lyricsfile::body(plain, synced, row.instrumental)?;
        (lf.get(at..)? == body).then(|| lf.get(..at).unwrap_or_default())
    });
    let lf_mode = match (head, &row.lyricsfile) {
        (Some(_), _) => mode::HEAD,
        (None, Some(_)) => mode::TEXT,
        (None, None) => mode::NULL,
    };
    if plain_mode == mode::DERIVED {
        counters.plain_derived.fetch_add(1, Ordering::Relaxed);
    }
    if synced_mode == mode::DERIVED {
        counters.synced_skeleton.fetch_add(1, Ordering::Relaxed);
    }
    match lf_mode {
        mode::HEAD => counters.lyricsfile_head.fetch_add(1, Ordering::Relaxed),
        mode::TEXT => counters.lyricsfile_text.fetch_add(1, Ordering::Relaxed),
        _ => 0,
    };
    let mut flags =
        (plain_mode << L_PLAIN_SHIFT) | (synced_mode << L_SYNCED_SHIFT) | (lf_mode << L_LF_SHIFT);
    if row.instrumental {
        flags |= L_INSTRUMENTAL;
    }
    if row.word_sync {
        flags |= L_WORD_SYNC;
    }
    if row.broken {
        flags |= L_BROKEN;
    }
    put_varint(out, u64::from(flags));
    if plain_mode == mode::TEXT {
        put_bytes(out, plain.unwrap_or_default().as_bytes());
    }
    match (synced_mode, &skeleton) {
        (mode::DERIVED, Some(s)) => put_bytes(out, s.as_bytes()),
        (mode::TEXT, _) => put_bytes(out, synced.unwrap_or_default().as_bytes()),
        _ => {}
    }
    match (lf_mode, head) {
        (mode::HEAD, Some(h)) => put_bytes(out, h.as_bytes()),
        (mode::TEXT, _) => put_bytes(
            out,
            row.lyricsfile.as_deref().unwrap_or_default().as_bytes(),
        ),
        _ => {}
    }
}

/// A temporary lyrics entry made final for its track: a Lyricsfile kept as its metadata part is dropped when that is
/// the track's. Returns the entry and whether LRCLIB's server can read the row.
fn finish_entry(
    entry: &[u8],
    track: &TrackRow,
    stats: &mut ImportStats,
) -> Option<(Vec<u8>, bool)> {
    let mut r = Reader::new(entry);
    let flags = u32::try_from(r.varint()?).ok()?;
    let broken = flags & L_BROKEN != 0;
    let mut flags = flags & !L_BROKEN;
    let texts_at = r.at;
    let mut skip = |stored: bool| -> Option<()> {
        if stored {
            r.string()?;
        }
        Some(())
    };
    skip((flags >> L_PLAIN_SHIFT) & 3 == mode::TEXT)?;
    skip((flags >> L_SYNCED_SHIFT) & 3 != mode::NULL)?;
    let texts_end = r.at;
    let lf_mode = (flags >> L_LF_SHIFT) & 3;
    let mut keep_lf = true;
    if lf_mode == mode::HEAD {
        let head = r.string()?;
        let meta = track_meta(
            track.name.as_deref(),
            track.artist.as_deref(),
            track.album.as_deref(),
            track.duration,
        );
        if lyricsfile::head(&meta, flags & L_INSTRUMENTAL != 0).as_deref() == Some(head) {
            stats.lyricsfile_built += 1;
            flags = (flags & !(3 << L_LF_SHIFT)) | (mode::DERIVED << L_LF_SHIFT);
            keep_lf = false;
        }
    }
    let mut out = Vec::with_capacity(entry.len());
    put_varint(&mut out, u64::from(flags));
    let rest = if keep_lf {
        entry.get(texts_at..)?
    } else {
        entry.get(texts_at..texts_end)?
    };
    out.extend_from_slice(rest);
    Some((out, broken))
}

/// A track record of the store.
fn encode_track(t: &TrackRow, broken: bool, out: &mut Vec<u8>, stats: &mut ImportStats) {
    let derived = |orig: &Option<String>| orig.as_deref().map(normalize);
    let mut flags = 0u8;
    let lowers = [
        (&t.name, &t.name_lower, T_NAME_LOWER),
        (&t.artist, &t.artist_lower, T_ARTIST_LOWER),
        (&t.album, &t.album_lower, T_ALBUM_LOWER),
    ];
    for (orig, lower, bit) in lowers {
        if derived(orig) != *lower {
            flags |= bit;
            stats.lower_exceptions += 1;
        }
    }
    for (s, bit) in [
        (&t.name, T_NAME),
        (&t.artist, T_ARTIST),
        (&t.album, T_ALBUM),
    ] {
        if s.is_some() {
            flags |= bit;
        }
    }
    if t.duration.is_some() {
        flags |= T_DURATION;
    }
    if broken || t.broken {
        flags |= T_BROKEN;
        stats.broken_tracks += 1;
    }
    put_varint(out, u64::from(t.id));
    out.push(flags);
    for s in [&t.name, &t.artist, &t.album].into_iter().flatten() {
        put_bytes(out, s.as_bytes());
    }
    for (orig, lower, _) in lowers {
        if derived(orig) != *lower {
            put_opt(out, lower.as_deref());
        }
    }
    if let Some(d) = t.duration {
        out.extend_from_slice(&d.to_le_bytes());
    }
}

/// A bucket made into store blocks.
#[derive(Default)]
struct BucketOut {
    /// (first sort key, tracks, compressed)
    track_blocks: Vec<(u64, u32, Vec<u8>)>,
    /// (entries, compressed)
    lyrics_blocks: Vec<(u32, Vec<u8>)>,
    ids: Vec<u32>,
    docs: Vec<search::Doc>,
    stats: ImportStats,
}

/// What becomes of a bucket's blocks.
enum Blocks<'d> {
    /// Compressed into the store.
    Store {
        tracks: zstd::bulk::Compressor<'d>,
        lyrics: zstd::bulk::Compressor<'d>,
    },
    /// Samples to train the dictionaries with: one 8 KB piece of every `every` lyrics block, and every track block.
    Samples {
        every: usize,
        seen: usize,
        tracks: &'d mut Vec<Vec<u8>>,
        lyrics: &'d mut Vec<Vec<u8>>,
    },
}

/// Makes a bucket's tracks, in order, into blocks; each block is compressed (or sampled) once it is full, so a
/// bucket's raw blocks are never all in memory.
struct BlockMaker<'d> {
    blocks: Blocks<'d>,
    out: BucketOut,
    track_keys: Vec<u64>,
    track_block: Vec<u8>,
    lyrics_block: Vec<u8>,
    lyrics_entries: u32,
}

impl<'d> BlockMaker<'d> {
    fn new(blocks: Blocks<'d>) -> Self {
        Self {
            blocks,
            out: BucketOut::default(),
            track_keys: Vec::with_capacity(TRACKS_PER_BLOCK),
            track_block: Vec::new(),
            lyrics_block: Vec::with_capacity(LYRICS_BLOCK_BYTES * 2),
            lyrics_entries: 0,
        }
    }

    /// Adds a track and its temporary lyrics entry (`None`: no lyrics).
    fn add(&mut self, key: u64, track: &TrackRow, entry: Option<&[u8]>) -> Result<(), ImportError> {
        let (entry, lyrics_broken) = if let Some(entry) = entry {
            finish_entry(entry, track, &mut self.out.stats)
                .ok_or_else(|| ImportError::Data("corrupt temporary file".to_owned()))?
        } else {
            if track.last_lyrics_id.is_some() {
                self.out.stats.lyrics_missing += 1;
            }
            (vec![0u8], false)
        };
        put_bytes(&mut self.lyrics_block, &entry);
        self.lyrics_entries += 1;
        if self.lyrics_block.len() >= LYRICS_BLOCK_BYTES {
            self.flush_lyrics()?;
        }
        encode_track(
            track,
            lyrics_broken,
            &mut self.track_block,
            &mut self.out.stats,
        );
        self.track_keys.push(key);
        if self.track_keys.len() == TRACKS_PER_BLOCK {
            self.flush_tracks()?;
        }
        self.out.ids.push(track.id);
        self.out.docs.push(search::Doc {
            track_id: i64::from(track.id),
            name_lower: track.name_lower.clone(),
            artist_lower: track.artist_lower.clone(),
            album_lower: track.album_lower.clone(),
        });
        Ok(())
    }

    fn flush_lyrics(&mut self) -> Result<(), ImportError> {
        if self.lyrics_entries == 0 {
            return Ok(());
        }
        let n = std::mem::take(&mut self.lyrics_entries);
        match &mut self.blocks {
            Blocks::Store { lyrics, .. } => {
                let compressed = lyrics
                    .compress(&self.lyrics_block)
                    .map_err(ImportError::Compress)?;
                self.out.lyrics_blocks.push((n, compressed));
            }
            Blocks::Samples {
                every,
                seen,
                lyrics,
                ..
            } => {
                if (*seen).is_multiple_of(*every) {
                    let end = self.lyrics_block.len().min(8 << 10);
                    lyrics.push(self.lyrics_block.get(..end).unwrap_or_default().to_vec());
                }
                *seen += 1;
            }
        }
        self.lyrics_block.clear();
        Ok(())
    }

    /// `[count][sort keys, u64 LE each][records]`
    fn flush_tracks(&mut self) -> Result<(), ImportError> {
        if self.track_keys.is_empty() {
            return Ok(());
        }
        let mut raw = Vec::with_capacity(4 + self.track_keys.len() * 8 + self.track_block.len());
        put_varint(&mut raw, self.track_keys.len() as u64);
        for k in &self.track_keys {
            raw.extend_from_slice(&k.to_le_bytes());
        }
        raw.extend_from_slice(&self.track_block);
        let first = self.track_keys.first().copied().unwrap_or_default();
        #[allow(clippy::cast_possible_truncation)] // TRACKS_PER_BLOCK
        let n = self.track_keys.len() as u32;
        match &mut self.blocks {
            Blocks::Store { tracks, .. } => {
                let compressed = tracks.compress(&raw).map_err(ImportError::Compress)?;
                self.out.track_blocks.push((first, n, compressed));
            }
            Blocks::Samples { tracks, .. } => tracks.push(raw),
        }
        self.track_keys.clear();
        self.track_block.clear();
        Ok(())
    }

    fn finish(mut self) -> Result<BucketOut, ImportError> {
        self.flush_lyrics()?;
        self.flush_tracks()?;
        Ok(self.out)
    }
}

/// Reads temporary lyrics entries into a map by lyrics id.
fn read_lyrics(path: &Path, into: &mut HashMap<u32, Vec<u8>>) -> Result<(), ImportError> {
    read_bucket(path, |record| {
        let mut r = Reader::new(record);
        let id = r
            .varint()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| ImportError::Data("corrupt temporary file".to_owned()))?;
        into.insert(id, record.get(r.at..).unwrap_or_default().to_vec());
        Ok(())
    })
}

/// Makes bucket `bucket` into blocks. Its lyrics are held in memory to be joined with the tracks; when they would take
/// more than `max_memory`, the bucket (an artist with very many tracks) is split by sort key into parts, its lyrics
/// spread over a temporary file per part, and the parts are made one after the other.
fn build_bucket(
    tmp: &Path,
    bucket: usize,
    blocks: Blocks<'_>,
    max_memory: u64,
) -> Result<BucketOut, ImportError> {
    let corrupt = || ImportError::Data("corrupt temporary file".to_owned());
    let mut tracks = Vec::new();
    read_bucket(&bucket_path(tmp, "tracks", bucket), |record| {
        tracks.push(decode_track_row(record).ok_or_else(corrupt)?);
        Ok(())
    })?;
    let mut keyed: Vec<(u64, TrackRow)> = tracks
        .into_iter()
        .map(|t| {
            (
                sort_key(t.artist_lower.as_deref(), t.name_lower.as_deref()),
                t,
            )
        })
        .collect();
    keyed.sort_unstable_by_key(|(key, t)| (*key, t.id));

    let lyrics_path = bucket_path(tmp, "lyrics", bucket);
    let lyrics_memory =
        std::fs::metadata(&lyrics_path).map_or(0, |m| m.len()) * MEMORY_PER_TEMP_BYTE;
    let parts = usize::try_from(lyrics_memory.div_ceil(max_memory.max(1)).max(1))
        .unwrap_or(1)
        .min(keyed.len().max(1));
    let mut maker = BlockMaker::new(blocks);
    let mut lyrics: HashMap<u32, Vec<u8>> = HashMap::new();
    if parts == 1 {
        read_lyrics(&lyrics_path, &mut lyrics)?;
        for (key, track) in &keyed {
            let entry = track.last_lyrics_id.and_then(|id| lyrics.get(&id));
            maker.add(*key, track, entry.map(Vec::as_slice))?;
        }
        return maker.finish();
    }

    tracing::info!(bucket, parts, "splitting a large bucket");
    maker.out.stats.split_buckets += 1;
    let bounds: Vec<usize> = (0..=parts).map(|p| p * keyed.len() / parts).collect();
    let mut part_of: HashMap<u32, Vec<usize>> = HashMap::new();
    for p in 0..parts {
        let (Some(&from), Some(&to)) = (bounds.get(p), bounds.get(p + 1)) else {
            continue;
        };
        for (_, track) in keyed.get(from..to).unwrap_or_default() {
            if let Some(id) = track.last_lyrics_id {
                let parts = part_of.entry(id).or_default();
                if !parts.contains(&p) {
                    parts.push(p);
                }
            }
        }
    }
    let part_path = |p: usize| tmp.join(format!("lyrics-{bucket:04}-part-{p:04}"));
    let mut files: Vec<BucketFile> = (0..parts)
        .map(|p| BucketFile::new(part_path(p), LYRICS_FRAME))
        .collect();
    read_bucket(&lyrics_path, |record| {
        let id = Reader::new(record)
            .varint()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(corrupt)?;
        for p in part_of.get(&id).map_or(&[][..], Vec::as_slice) {
            if let Some(file) = files.get_mut(*p) {
                file.push(record)?;
            }
        }
        Ok(())
    })?;
    for file in &mut files {
        file.flush()?;
    }
    drop(part_of);
    for p in 0..parts {
        lyrics.clear();
        read_lyrics(&part_path(p), &mut lyrics)?;
        let _ = std::fs::remove_file(part_path(p));
        let (Some(&from), Some(&to)) = (bounds.get(p), bounds.get(p + 1)) else {
            continue;
        };
        for (key, track) in keyed.get(from..to).unwrap_or_default() {
            let entry = track.last_lyrics_id.and_then(|id| lyrics.get(&id));
            maker.add(*key, track, entry.map(Vec::as_slice))?;
        }
    }
    maker.finish()
}

struct Dicts {
    tracks: zstd::dict::EncoderDictionary<'static>,
    lyrics: zstd::dict::EncoderDictionary<'static>,
}

/// Trains the compression dictionaries on samples of a few buckets, in pieces the size of what they compress
/// (zstd needs about a hundred times the dictionary's size of samples).
fn train(tmp: &Path, out: &Path, level: i32, max_memory: u64) -> Result<Dicts, ImportError> {
    let mut track_samples: Vec<Vec<u8>> = Vec::new();
    let mut lyrics_samples: Vec<Vec<u8>> = Vec::new();
    // From the size of the track buckets (written in the dump's order, so the same however often the import is
    // interrupted): roughly how many lyrics blocks the training buckets make (~40 tracks each, ~1.2 KB of temporary
    // file), and how many to skip between samples.
    let all: u64 = (0..BUCKETS)
        .step_by(BUCKETS / TRAINING_BUCKETS)
        .filter_map(|b| std::fs::metadata(bucket_path(tmp, "tracks", b)).ok())
        .map(|m| m.len())
        .sum();
    let blocks = usize::try_from(all / (48 << 10))
        .unwrap_or(usize::MAX)
        .max(1);
    let wanted = (LYRICS_DICT_BYTES * 100) / (8 << 10);
    let every = (blocks / wanted).max(1);
    for bucket in (0..BUCKETS).step_by(BUCKETS / TRAINING_BUCKETS) {
        let blocks = Blocks::Samples {
            every,
            seen: 0,
            tracks: &mut track_samples,
            lyrics: &mut lyrics_samples,
        };
        build_bucket(tmp, bucket, blocks, max_memory)?;
    }
    // Track blocks are small; a few hundred are enough.
    let step = (track_samples.len() / 400).max(1);
    let track_samples: Vec<Vec<u8>> = track_samples.into_iter().step_by(step).collect();
    let dict = |samples: &[Vec<u8>], size: usize| -> Vec<u8> {
        if samples.len() < 16 {
            return Vec::new();
        }
        zstd::dict::from_samples(samples, size).unwrap_or_default()
    };
    let tracks = dict(&track_samples, TRACKS_DICT_BYTES);
    let lyrics = dict(&lyrics_samples, LYRICS_DICT_BYTES);
    for (name, dict) in [(TRACKS_DICT, &tracks), (LYRICS_DICT, &lyrics)] {
        let path = out.join(name);
        file::write_atomic(&path, dict).map_err(io(&path))?;
    }
    Ok(Dicts {
        tracks: zstd::dict::EncoderDictionary::copy(&tracks, level),
        lyrics: zstd::dict::EncoderDictionary::copy(&lyrics, level),
    })
}

/// The dictionaries an interrupted import trained.
fn load_dicts(out: &Path, level: i32) -> Result<Dicts, ImportError> {
    let read = |name: &str| {
        let path = out.join(name);
        std::fs::read(&path).map_err(io(&path))
    };
    Ok(Dicts {
        tracks: zstd::dict::EncoderDictionary::copy(&read(TRACKS_DICT)?, level),
        lyrics: zstd::dict::EncoderDictionary::copy(&read(LYRICS_DICT)?, level),
    })
}

fn compressors(dicts: &Dicts) -> Result<Blocks<'_>, ImportError> {
    Ok(Blocks::Store {
        tracks: zstd::bulk::Compressor::with_prepared_dictionary(&dicts.tracks)
            .map_err(ImportError::Compress)?,
        lyrics: zstd::bulk::Compressor::with_prepared_dictionary(&dicts.lyrics)
            .map_err(ImportError::Compress)?,
    })
}

/// The output files of pass 2.
struct Writer {
    tracks: BufWriter<File>,
    tracks_index: BufWriter<File>,
    lyrics: BufWriter<File>,
    lyrics_index: BufWriter<File>,
    tracks_at: u64,
    tracks_index_at: u64,
    lyrics_at: u64,
    lyrics_index_at: u64,
    ordinal: u32,
    lyrics_ordinal: u32,
    track_blocks: u32,
    lyrics_blocks: u32,
    ids: Vec<u32>,
    search: search::IndexBuilder,
    /// Buckets from this one on are added to the search index (the ones before were committed to it).
    search_from: usize,
    out: PathBuf,
}

impl Writer {
    /// Opens the output files: new ones, or the ones of an interrupted import as they were at its last checkpoint.
    fn open(out: &Path, progress: &Progress, options: &ImportOptions) -> Result<Self, ImportError> {
        let checkpoint = progress.checkpoint.as_ref();
        let open = |name: &str, len: u64| -> Result<BufWriter<File>, ImportError> {
            let path = out.join(name);
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)
                .map_err(io(&path))?;
            f.set_len(len).map_err(io(&path))?;
            f.seek(SeekFrom::End(0)).map_err(io(&path))?;
            Ok(BufWriter::with_capacity(1 << 20, f))
        };
        let search_dir = out.join(SEARCH);
        let (search, search_from) = if let Some(c) = checkpoint {
            let (search, payload) =
                search::IndexBuilder::open(&search_dir, options.threads, options.index_memory)
                    .map_err(|e| ImportError::Search(e.to_string()))?;
            // Committed past the checkpoint (interrupted before the checkpoint was recorded): those buckets are in
            // the index already.
            let committed = payload.and_then(|p| p.parse::<usize>().ok()).unwrap_or(0);
            (search, committed.max(c.buckets))
        } else {
            let _ = std::fs::remove_dir_all(&search_dir);
            let search =
                search::IndexBuilder::create(&search_dir, options.threads, options.index_memory)
                    .map_err(|e| ImportError::Search(e.to_string()))?;
            (search, 0)
        };
        let ids = match checkpoint {
            Some(c) => {
                let path = out.join(ids_partial(c.buckets));
                let bytes = std::fs::read(&path).map_err(io(&path))?;
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| u32::from_le_bytes(*b))
                    .collect()
            }
            None => vec![0; progress.max_track_id as usize + 1],
        };
        let c = checkpoint.cloned().unwrap_or(Checkpoint {
            buckets: 0,
            tracks: 0,
            tracks_index: 0,
            lyrics: 0,
            lyrics_index: 0,
            ordinal: 0,
            lyrics_ordinal: 0,
            track_blocks: 0,
            lyrics_blocks: 0,
            stats: ImportStats::default(),
        });
        Ok(Self {
            tracks: open(TRACKS, c.tracks)?,
            tracks_index: open(TRACKS_INDEX, c.tracks_index)?,
            lyrics: open(LYRICS, c.lyrics)?,
            lyrics_index: open(LYRICS_INDEX, c.lyrics_index)?,
            tracks_at: c.tracks,
            tracks_index_at: c.tracks_index,
            lyrics_at: c.lyrics,
            lyrics_index_at: c.lyrics_index,
            ordinal: c.ordinal,
            lyrics_ordinal: c.lyrics_ordinal,
            track_blocks: c.track_blocks,
            lyrics_blocks: c.lyrics_blocks,
            ids,
            search,
            search_from,
            out: out.to_owned(),
        })
    }

    /// Writes bucket `index`.
    fn write(
        &mut self,
        bucket: BucketOut,
        index: usize,
        stats: &mut ImportStats,
    ) -> Result<(), ImportError> {
        let path = self.out.clone();
        let err = |x| ImportError::Io(path.clone(), x);
        let mut ordinal = self.ordinal;
        for (key, n, block) in &bucket.track_blocks {
            // first key, first ordinal, offset
            let mut entry = [0u8; 20];
            entry[..8].copy_from_slice(&key.to_le_bytes());
            entry[8..12].copy_from_slice(&ordinal.to_le_bytes());
            entry[12..].copy_from_slice(&self.tracks_at.to_le_bytes());
            self.tracks_index.write_all(&entry).map_err(err)?;
            self.tracks_index_at += 20;
            self.tracks.write_all(block).map_err(err)?;
            self.tracks_at += block.len() as u64;
            ordinal += n;
            self.track_blocks += 1;
        }
        for (n, block) in &bucket.lyrics_blocks {
            let mut entry = [0u8; 12];
            entry[..4].copy_from_slice(&self.lyrics_ordinal.to_le_bytes());
            entry[4..].copy_from_slice(&self.lyrics_at.to_le_bytes());
            self.lyrics_index.write_all(&entry).map_err(err)?;
            self.lyrics_index_at += 12;
            self.lyrics.write_all(block).map_err(err)?;
            self.lyrics_at += block.len() as u64;
            self.lyrics_ordinal += n;
            self.lyrics_blocks += 1;
        }
        for id in &bucket.ids {
            let slot = self
                .ids
                .get_mut(*id as usize)
                .ok_or_else(|| ImportError::Data("track id".to_owned()))?;
            if *slot != 0 {
                return Err(ImportError::Data(format!("track {id} twice")));
            }
            self.ordinal += 1;
            *slot = self.ordinal;
        }
        if self.ordinal != ordinal || self.lyrics_ordinal != ordinal {
            return Err(ImportError::Data("blocks out of step".to_owned()));
        }
        if index >= self.search_from {
            for doc in bucket.docs {
                self.search
                    .add(&doc)
                    .map_err(|x| ImportError::Search(x.to_string()))?;
            }
        }
        merge(stats, &bucket.stats);
        Ok(())
    }

    /// Makes everything written so far durable, as of `buckets` buckets.
    fn checkpoint(
        &mut self,
        buckets: usize,
        stats: &ImportStats,
    ) -> Result<Checkpoint, ImportError> {
        let path = self.out.clone();
        let err = |x| ImportError::Io(path.clone(), x);
        for w in [
            &mut self.tracks,
            &mut self.tracks_index,
            &mut self.lyrics,
            &mut self.lyrics_index,
        ] {
            w.flush().map_err(err)?;
            w.get_ref().sync_all().map_err(err)?;
        }
        self.write_ids(&ids_partial(buckets))?;
        self.search
            .commit(&buckets.to_string())
            .map_err(|x| ImportError::Search(x.to_string()))?;
        Ok(Checkpoint {
            buckets,
            tracks: self.tracks_at,
            tracks_index: self.tracks_index_at,
            lyrics: self.lyrics_at,
            lyrics_index: self.lyrics_index_at,
            ordinal: self.ordinal,
            lyrics_ordinal: self.lyrics_ordinal,
            track_blocks: self.track_blocks,
            lyrics_blocks: self.lyrics_blocks,
            stats: stats.clone(),
        })
    }

    /// Writes the id map to `name`, whole or not at all.
    fn write_ids(&self, name: &str) -> Result<(), ImportError> {
        let path = self.out.join(name);
        let tmp = self.out.join(format!("{name}.tmp"));
        let mut f = BufWriter::with_capacity(1 << 20, File::create(&tmp).map_err(io(&tmp))?);
        for id in &self.ids {
            f.write_all(&id.to_le_bytes()).map_err(io(&tmp))?;
        }
        f.flush().map_err(io(&tmp))?;
        f.get_ref().sync_all().map_err(io(&tmp))?;
        drop(f);
        std::fs::rename(&tmp, &path).map_err(io(&path))?;
        file::sync_dir(&self.out).map_err(io(&self.out))
    }

    fn finish(mut self) -> Result<(u32, u32, u32), ImportError> {
        let path = self.out.clone();
        let err = |x| ImportError::Io(path.clone(), x);
        // Sentinels: the end of the last block.
        let mut entry = [0u8; 20];
        entry[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        entry[8..12].copy_from_slice(&self.ordinal.to_le_bytes());
        entry[12..].copy_from_slice(&self.tracks_at.to_le_bytes());
        self.tracks_index.write_all(&entry).map_err(err)?;
        let mut entry = [0u8; 12];
        entry[..4].copy_from_slice(&self.lyrics_ordinal.to_le_bytes());
        entry[4..].copy_from_slice(&self.lyrics_at.to_le_bytes());
        self.lyrics_index.write_all(&entry).map_err(err)?;
        for w in [
            &mut self.tracks,
            &mut self.tracks_index,
            &mut self.lyrics,
            &mut self.lyrics_index,
        ] {
            w.flush().map_err(err)?;
            w.get_ref().sync_all().map_err(err)?;
        }
        self.write_ids(IDS)?;
        self.ids = Vec::new();
        tracing::info!("merging the search index");
        self.search
            .finish()
            .map_err(|x| ImportError::Search(x.to_string()))?;
        Ok((self.ordinal, self.track_blocks, self.lyrics_blocks))
    }
}

fn merge(into: &mut ImportStats, s: &ImportStats) {
    into.lyricsfile_built += s.lyricsfile_built;
    into.lyricsfile_head = into.lyricsfile_head.saturating_sub(s.lyricsfile_built);
    into.lower_exceptions += s.lower_exceptions;
    into.broken_tracks += s.broken_tracks;
    into.lyrics_missing += s.lyrics_missing;
    into.split_buckets += s.split_buckets;
}

/// The memory each bucket takes while it is built, estimated from its temporary files.
fn bucket_weights(tmp: &Path) -> Vec<u64> {
    (0..BUCKETS)
        .map(|b| {
            ["tracks", "lyrics"]
                .iter()
                .filter_map(|kind| std::fs::metadata(bucket_path(tmp, kind, b)).ok())
                .map(|m| m.len() * MEMORY_PER_TEMP_BYTE)
                .sum()
        })
        .collect()
}

/// Pass 2's buckets: made in parallel, written in order. A bucket starts when it is within `window` of the next one
/// to write, and when the buckets being made take less than `budget` (estimated from the size of their temporary
/// files, at most `max_bucket` each: larger buckets are made in parts).
struct Schedule {
    weights: Vec<u64>,
    window: usize,
    budget: u64,
    max_bucket: u64,
    next: AtomicUsize,
    /// (buckets written, memory of the buckets being made)
    admission: Mutex<(usize, u64)>,
    admission_cv: Condvar,
    done: Mutex<BTreeMap<usize, Result<BucketOut, ImportError>>>,
    done_cv: Condvar,
    failed: AtomicBool,
}

impl Schedule {
    fn new(tmp: &Path, threads: usize, budget: u64, max_bucket: u64, start: usize) -> Self {
        let weights = bucket_weights(tmp);
        tracing::info!(
            largest_bucket_mb = weights.iter().copied().max().unwrap_or_default() >> 20,
            all_buckets_mb = weights.iter().sum::<u64>() >> 20,
            "sorting buckets"
        );
        Self {
            weights,
            window: threads * 2,
            budget,
            max_bucket,
            next: AtomicUsize::new(start),
            admission: Mutex::new((start, 0)),
            admission_cv: Condvar::new(),
            done: Mutex::new(BTreeMap::new()),
            done_cv: Condvar::new(),
            failed: AtomicBool::new(false),
        }
    }

    /// A worker thread: makes buckets until there are none left.
    fn work(&self, tmp: &Path, dicts: &Dicts, stop: &AtomicBool) {
        loop {
            let bucket = self.next.fetch_add(1, Ordering::Relaxed);
            if bucket >= BUCKETS || self.failed.load(Ordering::Relaxed) {
                return;
            }
            let weight = self
                .weights
                .get(bucket)
                .copied()
                .unwrap_or_default()
                .min(self.max_bucket);
            if let Ok(mut a) = self.admission.lock() {
                while (bucket >= a.0 + self.window || (a.1 > 0 && a.1 + weight > self.budget))
                    && !self.failed.load(Ordering::Relaxed)
                {
                    a = match self.admission_cv.wait(a) {
                        Ok(a) => a,
                        Err(_) => return,
                    };
                }
                a.1 += weight;
            }
            let result = if stop.load(Ordering::Relaxed) {
                Err(ImportError::Stopped)
            } else {
                compressors(dicts)
                    .and_then(|blocks| build_bucket(tmp, bucket, blocks, self.max_bucket))
            };
            // What is left of the bucket (its compressed blocks) is small.
            if let Ok(mut a) = self.admission.lock() {
                a.1 = a.1.saturating_sub(weight);
                self.admission_cv.notify_all();
            }
            if let Ok(mut d) = self.done.lock() {
                d.insert(bucket, result);
                self.done_cv.notify_all();
            }
        }
    }

    /// Waits for bucket `bucket` to be made.
    fn take(&self, bucket: usize) -> Result<BucketOut, ImportError> {
        let poisoned = || ImportError::Data("a worker failed".to_owned());
        let mut d = self.done.lock().map_err(|_| poisoned())?;
        loop {
            if let Some(item) = d.remove(&bucket) {
                return item;
            }
            d = self.done_cv.wait(d).map_err(|_| poisoned())?;
        }
    }

    fn written(&self, bucket: usize) {
        if let Ok(mut a) = self.admission.lock() {
            a.0 = bucket + 1;
            self.admission_cv.notify_all();
        }
    }

    fn fail(&self) {
        self.failed.store(true, Ordering::Relaxed);
        self.admission_cv.notify_all();
    }
}

fn pass2(
    tmp: &Path,
    out: &Path,
    progress: &mut Progress,
    options: &ImportOptions,
    stop: &AtomicBool,
) -> Result<Meta, ImportError> {
    // A bucket larger than its share of the memory is made in parts.
    let max_bucket = (options.memory as u64 / options.threads.max(1) as u64).max(1 << 20);
    let dicts = if progress.dictionaries {
        load_dicts(out, options.level)?
    } else {
        let dicts = train(tmp, out, options.level, max_bucket)?;
        progress.dictionaries = true;
        write_progress(out, progress)?;
        dicts
    };
    let mut stats = progress
        .checkpoint
        .as_ref()
        .map_or_else(|| progress.stats.clone(), |c| c.stats.clone());
    let start = progress.checkpoint.as_ref().map_or(0, |c| c.buckets);
    let mut writer = Writer::open(out, progress, options)?;

    let threads = options.threads.max(1);
    let every = options.checkpoint_every.max(1);
    let schedule = Schedule::new(tmp, threads, options.memory as u64, max_bucket, start);
    std::thread::scope(|scope| -> Result<(), ImportError> {
        for _ in 0..threads {
            scope.spawn(|| schedule.work(tmp, &dicts, stop));
        }
        let mut result = Ok(());
        let mut written = Vec::new();
        for bucket in start..BUCKETS {
            if let Err(e) = schedule
                .take(bucket)
                .and_then(|b| writer.write(b, bucket, &mut stats))
            {
                result = Err(e);
                break;
            }
            schedule.written(bucket);
            written.push(bucket);
            if (bucket + 1).is_multiple_of(every) && bucket + 1 < BUCKETS {
                // The buckets' temporary files are removed only once what they became is durable.
                let saved = writer.checkpoint(bucket + 1, &stats).and_then(|c| {
                    progress.checkpoint = Some(c);
                    write_progress(out, progress)
                });
                remove_ids_partials(out, Some(bucket + 1));
                if let Err(e) = saved {
                    result = Err(e);
                    break;
                }
                for b in written.drain(..) {
                    for kind in ["tracks", "lyrics"] {
                        let _ = std::fs::remove_file(bucket_path(tmp, kind, b));
                    }
                }
            }
            if bucket % 64 == 63 {
                tracing::info!(
                    buckets = bucket + 1,
                    of = BUCKETS,
                    rss_mb = rss_mb(),
                    "writing the store"
                );
            }
        }
        if result.is_err() {
            schedule.fail();
        }
        result
    })?;

    let (tracks, track_blocks, lyrics_blocks) = writer.finish()?;
    Ok(Meta {
        format: FORMAT_VERSION,
        derivations: super::derivations(),
        tracks,
        max_track_id: progress.max_track_id,
        track_blocks,
        lyrics_blocks,
        stats,
    })
}
