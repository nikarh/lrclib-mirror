//! LRCLIB's public database dumps: the listing, and the download as a stream (gunzipped by the reader).

use std::io::Read;
use std::time::Duration;

use bytes::Buf as _;
use serde::Deserialize;
use tokio::sync::mpsc;

/// A download that delivers nothing for this long is abandoned.
const STALL_TIMEOUT: Duration = Duration::from_secs(300);
const PROGRESS_EVERY: u64 = 5 << 30;

#[derive(Debug, thiserror::Error)]
pub enum DumpError {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("the download stalled for {}s", STALL_TIMEOUT.as_secs())]
    Stalled,
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
#[must_use]
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

/// The name of the store made from a dump: its key without the extensions.
#[must_use]
pub fn store_name(key: &str) -> &str {
    key.strip_suffix(".sqlite3.gz").unwrap_or(key)
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

/// The body of a download, read by a blocking reader while an async task receives it. A stalled download, or one
/// that fails, ends the reader with an error.
pub struct Download {
    rx: mpsc::Receiver<std::io::Result<bytes::Bytes>>,
    chunk: bytes::Bytes,
}

impl Read for Download {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while !self.chunk.has_remaining() {
            match self.rx.blocking_recv() {
                Some(Ok(chunk)) => self.chunk = chunk,
                Some(Err(e)) => return Err(e),
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.chunk.remaining());
        self.chunk
            .copy_to_slice(buf.get_mut(..n).unwrap_or_default());
        Ok(n)
    }
}

/// Starts downloading `url`. The body is read from the returned [`Download`] (on a blocking thread).
pub async fn download(client: &reqwest::Client, url: &str) -> Result<Download, DumpError> {
    let mut response = client.get(url).send().await?.error_for_status()?;
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let mut received = 0u64;
        let mut next_report = PROGRESS_EVERY;
        loop {
            let chunk = match tokio::time::timeout(STALL_TIMEOUT, response.chunk()).await {
                Err(_) => Err(std::io::Error::other(DumpError::Stalled)),
                Ok(Err(e)) => Err(std::io::Error::other(e)),
                Ok(Ok(None)) => return,
                Ok(Ok(Some(chunk))) => Ok(chunk),
            };
            if let Ok(chunk) = &chunk {
                received += chunk.len() as u64;
                if received >= next_report {
                    tracing::info!(downloaded_gb = received >> 30, "downloading");
                    next_report += PROGRESS_EVERY;
                }
            }
            let failed = chunk.is_err();
            // The reader is gone: it failed or stopped.
            if tx.send(chunk).await.is_err() || failed {
                return;
            }
        }
    });
    Ok(Download {
        rx,
        chunk: bytes::Bytes::new(),
    })
}

/// A dump, gunzipped (a broken or cut stream fails: gzip's trailer has the length and a CRC).
pub fn gunzip<R: Read>(input: R) -> impl Read {
    flate2::read::MultiGzDecoder::new(std::io::BufReader::with_capacity(1 << 20, input))
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
            store_name("lrclib-db-dump-20261030T182259Z.sqlite3.gz"),
            "lrclib-db-dump-20261030T182259Z"
        );
    }

    #[test]
    fn gunzips_and_rejects_cut_streams() {
        use std::io::Write as _;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&vec![7u8; 100_000]).unwrap();
        let gz = gz.finish().unwrap();
        let mut out = Vec::new();
        gunzip(&gz[..]).read_to_end(&mut out).unwrap();
        assert_eq!(out, vec![7u8; 100_000]);
        assert!(
            gunzip(&gz[..gz.len() - 4])
                .read_to_end(&mut Vec::new())
                .is_err()
        );
    }
}
