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

/// A dropped connection is resumed (an HTTP range from where it stopped) this many times.
const RESUMES: u32 = 5;

/// Starts downloading `url`. The body is read from the returned [`Download`] (on a blocking thread). A connection
/// that drops or stalls is resumed where it stopped, a few times: the reader doesn't see it.
pub async fn download(client: &reqwest::Client, url: &str) -> Result<Download, DumpError> {
    let response = client.get(url).send().await?.error_for_status()?;
    let (tx, rx) = mpsc::channel(64);
    let (client, url) = (client.clone(), url.to_owned());
    tokio::spawn(async move {
        let mut response = response;
        let mut received = 0u64;
        let mut next_report = PROGRESS_EVERY;
        let mut resumes = 0;
        loop {
            let chunk = match tokio::time::timeout(STALL_TIMEOUT, response.chunk()).await {
                Err(_) => Err(std::io::Error::other(DumpError::Stalled)),
                Ok(Err(e)) => Err(std::io::Error::other(e)),
                Ok(Ok(None)) => return,
                Ok(Ok(Some(chunk))) => Ok(chunk),
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(e) if resumes < RESUMES => {
                    resumes += 1;
                    tracing::warn!(error = %e, received_mb = received >> 20, resumes, "the download broke; resuming");
                    tokio::time::sleep(Duration::from_secs(10 * u64::from(resumes))).await;
                    match resume(&client, &url, received).await {
                        Ok(r) => response = r,
                        Err(e) => {
                            let _ = tx.send(Err(std::io::Error::other(e))).await;
                            return;
                        }
                    }
                    continue;
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };
            received += chunk.len() as u64;
            if received >= next_report {
                tracing::info!(downloaded_gb = received >> 30, "downloading");
                next_report += PROGRESS_EVERY;
            }
            // The reader is gone: it failed or stopped.
            if tx.send(Ok(chunk)).await.is_err() {
                return;
            }
        }
    });
    Ok(Download {
        rx,
        chunk: bytes::Bytes::new(),
    })
}

/// The rest of `url` from byte `from`; an error when the server doesn't answer with exactly that.
async fn resume(
    client: &reqwest::Client,
    url: &str,
    from: u64,
) -> Result<reqwest::Response, String> {
    let response = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={from}-"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let starts_at = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes "))
        .and_then(|v| v.split('-').next())
        .and_then(|v| v.parse::<u64>().ok());
    if response.status() == reqwest::StatusCode::PARTIAL_CONTENT && starts_at == Some(from) {
        Ok(response)
    } else {
        Err(format!(
            "can't resume the download: the server answered {}",
            response.status()
        ))
    }
}

/// A dump, gunzipped on a thread of its own (a broken or cut stream fails: gzip's trailer has the length and a CRC).
pub fn gunzip<R: Read + Send + 'static>(input: R) -> impl Read + Send {
    read_ahead(flate2::read::MultiGzDecoder::new(
        std::io::BufReader::with_capacity(1 << 20, input),
    ))
}

/// Reads `input` on a thread of its own, a few MB ahead of the reader: the reading (or unpacking) and what is done
/// with what was read overlap.
pub fn read_ahead<R: Read + Send + 'static>(mut input: R) -> impl Read + Send {
    let (tx, rx) = std::sync::mpsc::sync_channel::<std::io::Result<Vec<u8>>>(8);
    std::thread::spawn(move || {
        loop {
            let mut chunk = vec![0u8; 1 << 20];
            let mut filled = 0;
            let result = loop {
                match input.read(chunk.get_mut(filled..).unwrap_or_default()) {
                    Ok(0) => break Ok(()),
                    Ok(n) => {
                        filled += n;
                        if filled == chunk.len() {
                            break Ok(());
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => break Err(e),
                }
            };
            chunk.truncate(filled);
            let end = filled == 0;
            let sent = match result {
                Ok(()) if end => return,
                Ok(()) => tx.send(Ok(chunk)),
                Err(e) => {
                    let _ = tx.send(Ok(chunk));
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            // The reader is gone.
            if sent.is_err() {
                return;
            }
        }
    });
    ReadAhead {
        rx,
        chunk: Vec::new(),
        at: 0,
    }
}

struct ReadAhead {
    rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    chunk: Vec<u8>,
    at: usize,
}

impl Read for ReadAhead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.at >= self.chunk.len() {
            match self.rx.recv() {
                Ok(Ok(chunk)) => {
                    self.chunk = chunk;
                    self.at = 0;
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(0),
            }
        }
        let rest = self.chunk.get(self.at..).unwrap_or_default();
        let n = rest.len().min(buf.len());
        if let (Some(dst), Some(src)) = (buf.get_mut(..n), rest.get(..n)) {
            dst.copy_from_slice(src);
        }
        self.at += n;
        Ok(n)
    }
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
        gunzip(std::io::Cursor::new(gz.clone()))
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, vec![7u8; 100_000]);
        assert!(
            gunzip(std::io::Cursor::new(gz[..gz.len() - 4].to_vec()))
                .read_to_end(&mut Vec::new())
                .is_err()
        );
    }
}
