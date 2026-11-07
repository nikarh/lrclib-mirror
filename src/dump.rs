//! LRCLIB's public database dumps: the listing, the download (gunzipped while it streams) and a sanity check.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::mpsc;

/// A download that delivers nothing for this long is abandoned.
const STALL_TIMEOUT: Duration = Duration::from_secs(300);
const PROGRESS_EVERY: u64 = 5 << 30;

#[derive(Debug, thiserror::Error)]
pub enum DumpError {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("{0}: {1}")]
    Sqlite(PathBuf, rusqlite::Error),
    #[error("the download stalled for {}s", STALL_TIMEOUT.as_secs())]
    Stalled,
    #[error("{0} is not an LRCLIB database: {1}")]
    NotLrclib(PathBuf, String),
}

#[derive(Deserialize)]
struct Listing {
    objects: Vec<Object>,
}

#[derive(Deserialize)]
struct Object {
    key: String,
}

/// A dump's name in the listing, e.g. `lrclib-db-dump-20261030T182259Z.sqlite3.gz`. Only names of this shape are
/// accepted (they become file names), and they sort by date.
pub fn is_dump_key(key: &str) -> bool {
    key.strip_prefix("lrclib-db-dump-")
        .and_then(|k| k.strip_suffix(".sqlite3.gz"))
        .is_some_and(|stamp| {
            !stamp.is_empty()
                && stamp
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == 'T' || c == 'Z')
        })
}

/// The file a dump is unpacked to.
pub fn db_file_name(key: &str) -> &str {
    key.strip_suffix(".gz").unwrap_or(key)
}

/// The newest dump in the listing.
pub async fn latest(
    client: &reqwest::Client,
    listing_url: &str,
) -> Result<Option<String>, DumpError> {
    let listing: Listing = client
        .get(listing_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(listing
        .objects
        .into_iter()
        .map(|o| o.key)
        .filter(|k| is_dump_key(k))
        .max())
}

/// Downloads `url` and writes it gunzipped to `dest`. A broken transfer fails (gzip's length and CRC check covers
/// the end), so a complete file is never confused with a cut one.
pub async fn download(client: &reqwest::Client, url: &str, dest: &Path) -> Result<(), DumpError> {
    let mut response = client.get(url).send().await?.error_for_status()?;

    let (tx, rx) = mpsc::channel::<bytes::Bytes>(64);
    let path = dest.to_owned();
    let writer = tokio::task::spawn_blocking(move || write_gunzipped(&path, rx));

    let mut received = 0u64;
    let mut next_report = PROGRESS_EVERY;
    let streamed = async {
        loop {
            let Some(chunk) = tokio::time::timeout(STALL_TIMEOUT, response.chunk())
                .await
                .map_err(|_| DumpError::Stalled)??
            else {
                return Ok::<(), DumpError>(());
            };
            received += chunk.len() as u64;
            if received >= next_report {
                tracing::info!(downloaded_gb = received >> 30, "downloading");
                next_report += PROGRESS_EVERY;
            }
            if tx.send(chunk).await.is_err() {
                // The writer failed; its error is reported below.
                return Ok(());
            }
        }
    }
    .await;
    drop(tx);

    let written = writer
        .await
        .map_err(|e| DumpError::Io(dest.to_owned(), std::io::Error::other(e)))?;
    streamed?;
    written
}

fn write_gunzipped(path: &Path, mut rx: mpsc::Receiver<bytes::Bytes>) -> Result<(), DumpError> {
    let io = |e| DumpError::Io(path.to_owned(), e);
    let file = std::fs::File::create(path).map_err(io)?;
    let mut decoder =
        flate2::write::GzDecoder::new(std::io::BufWriter::with_capacity(1 << 20, file));
    while let Some(chunk) = rx.blocking_recv() {
        decoder.write_all(&chunk).map_err(io)?;
    }
    // Fails on a stream that ended early: the trailer (CRC and length) is missing or wrong.
    let file = decoder
        .finish()
        .map_err(io)?
        .into_inner()
        .map_err(|e| io(e.into_error()))?;
    file.sync_all().map_err(io)
}

/// Checks that `path` is an LRCLIB database with tracks.
pub fn check(path: &Path) -> Result<(), DumpError> {
    let sqlite = |e| DumpError::Sqlite(path.to_owned(), e);
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(sqlite)?;
    let tables: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ('tracks', 'lyrics')",
            [],
            |r| r.get(0),
        )
        .map_err(sqlite)?;
    if tables != 2 {
        return Err(DumpError::NotLrclib(
            path.to_owned(),
            "no tracks and lyrics tables".to_owned(),
        ));
    }
    let max: Option<i64> = db
        .query_row("SELECT max(id) FROM tracks", [], |r| r.get(0))
        .map_err(sqlite)?;
    if max.unwrap_or(0) <= 0 {
        return Err(DumpError::NotLrclib(
            path.to_owned(),
            "no tracks".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_keys() {
        assert!(is_dump_key("lrclib-db-dump-20261030T182259Z.sqlite3.gz"));
        assert!(!is_dump_key("lrclib-db-dump-.sqlite3.gz"));
        assert!(!is_dump_key("lrclib-db-dump-../x.sqlite3.gz"));
        assert!(!is_dump_key("other.sqlite3.gz"));
        assert_eq!(
            db_file_name("lrclib-db-dump-20261030T182259Z.sqlite3.gz"),
            "lrclib-db-dump-20261030T182259Z.sqlite3"
        );
    }

    #[tokio::test]
    async fn gunzips_and_rejects_cut_streams() {
        let dir = std::env::temp_dir().join(format!("lrclib-mirror-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&vec![7u8; 100_000]).unwrap();
        let gz = gz.finish().unwrap();

        for (name, data, ok) in [
            ("whole", &gz[..], true),
            ("cut", &gz[..gz.len() - 4], false),
        ] {
            let (tx, rx) = mpsc::channel(4);
            let path = dir.join(name);
            let writer = tokio::task::spawn_blocking(move || write_gunzipped(&path, rx));
            for chunk in data.chunks(1000) {
                tx.send(bytes::Bytes::copy_from_slice(chunk)).await.unwrap();
            }
            drop(tx);
            assert_eq!(writer.await.unwrap().is_ok(), ok, "{name}");
        }
        assert_eq!(
            std::fs::read(dir.join("whole")).unwrap(),
            vec![7u8; 100_000]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
