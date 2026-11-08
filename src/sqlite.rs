//! Reads the rows of an SQLite database file from a stream, in one pass, without the file on disk.
//!
//! LRCLIB's dumps are ~270 GB unpacked. Rather than unpacking a dump to disk and querying it, the importer reads the
//! gzip stream as it downloads and picks the table rows out of the pages as they go by:
//!
//! - A table b-tree leaf page (type `0x0d`) holds whole rows, or the first part of a row whose record overflows into a
//!   chain of overflow pages. Leaves are recognized by their type byte and their structure.
//! - Overflow pages carry no type: they are known only from the row (or the previous overflow page) pointing to them.
//!   Most come after the leaf that points to them, but not all; pages that arrive before anything claims them are set
//!   aside in a spill file until they are claimed.
//! - Interior and index pages are of no use here. An index interior page (`0x02`) can't be told from an overflow page
//!   whose next-page pointer starts with `0x02`, so those are set aside too; the others are skipped.
//!
//! What is not supported, and refused: free pages (a stale leaf on the free list would bring back rows that no longer
//! exist; dumps made by `VACUUM INTO` have none), auto-vacuum (pointer map pages), text encodings other than UTF-8, and
//! files so large that an overflow page's next-page pointer could look like a table leaf's type byte.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::FileExt as _;
use std::path::Path;

const HEADER_MAGIC: &[u8; 16] = b"SQLite format 3\0";
const TABLE_LEAF: u8 = 0x0d;
const TABLE_INTERIOR: u8 = 0x05;
const INDEX_LEAF: u8 = 0x0a;
const INDEX_INTERIOR: u8 = 0x02;

#[derive(Debug, thiserror::Error)]
pub enum SqliteError {
    #[error("reading the database: {0}")]
    Io(#[from] std::io::Error),
    #[error("not an SQLite database")]
    NotSqlite,
    #[error("unsupported database: {0}")]
    Unsupported(String),
    #[error("malformed database: {0}")]
    Malformed(String),
    #[error(transparent)]
    Row(Box<dyn std::error::Error + Send + Sync>),
}

/// What a scan saw.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScanStats {
    pub pages: u64,
    pub rows: u64,
    /// Pages that arrived before the row pointing to them (at most this many were set aside at once).
    pub max_early_pages: usize,
}

/// A column value of a record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value<'a> {
    Null,
    Int(i64),
    Real(f64),
    Text(&'a [u8]),
    Blob(&'a [u8]),
}

/// Decodes a record (the payload of a row) into its column values.
pub fn decode_record(payload: &[u8]) -> Result<Vec<Value<'_>>, SqliteError> {
    let bad = || SqliteError::Malformed("bad record".to_owned());
    let (header_len, mut at) = varint(payload).ok_or_else(bad)?;
    let header_len = usize::try_from(header_len).map_err(|_| bad())?;
    let header = payload.get(..header_len).ok_or_else(bad)?;
    let mut body = header_len;
    let mut values = Vec::with_capacity(16);
    while at < header_len {
        let (serial, n) = varint(header.get(at..).ok_or_else(bad)?).ok_or_else(bad)?;
        at += n;
        let len = serial_len(serial);
        let data = payload.get(body..body + len).ok_or_else(bad)?;
        body += len;
        values.push(match serial {
            0 => Value::Null,
            1..=6 => Value::Int(be_int(data)),
            7 => Value::Real(f64::from_bits(be_int(data).cast_unsigned())),
            8 => Value::Int(0),
            9 => Value::Int(1),
            10 | 11 => return Err(bad()),
            n if n % 2 == 0 => Value::Blob(data),
            _ => Value::Text(data),
        });
    }
    Ok(values)
}

/// The column serial types of a record, from its header alone (which is always in the local part of a row).
#[must_use]
pub fn record_types(payload: &[u8]) -> Option<Vec<u64>> {
    let (header_len, mut at) = varint(payload)?;
    let header = payload.get(..usize::try_from(header_len).ok()?)?;
    let mut types = Vec::with_capacity(16);
    while at < header.len() {
        let (serial, n) = varint(header.get(at..)?)?;
        types.push(serial);
        at += n;
    }
    Some(types)
}

fn serial_len(serial: u64) -> usize {
    match serial {
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 | 7 => 8,
        n if n >= 12 => usize::try_from((n - 12) / 2).unwrap_or(usize::MAX),
        _ => 0,
    }
}

/// A big-endian two's complement integer of 1 to 8 bytes.
fn be_int(data: &[u8]) -> i64 {
    let mut v: i64 = if data.first().is_some_and(|b| b & 0x80 != 0) {
        -1
    } else {
        0
    };
    for &b in data {
        v = (v << 8) | i64::from(b);
    }
    v
}

/// SQLite's variable-length integer: 1 to 9 bytes, big-endian, 7 bits per byte (8 in the ninth).
fn varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for i in 0..9 {
        let c = *b.get(i)?;
        if i == 8 {
            return Some(((v << 8) | u64::from(c), 9));
        }
        v = (v << 7) | u64::from(c & 0x7f);
        if c & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

fn be_u16(b: &[u8], at: usize) -> Option<usize> {
    Some(usize::from(u16::from_be_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
    ])))
}

fn be_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// A row whose record continues in overflow pages not seen yet.
struct Pending {
    rowid: i64,
    total: usize,
    data: Vec<u8>,
}

/// Pages that arrived before the row (or overflow page) pointing to them, kept in a temporary file. Most are claimed
/// soon after; their slots are reused, so the file stays as large as the most pages set aside at once.
struct Spill {
    file: File,
    page_size: u64,
    slots: u64,
    free: Vec<u64>,
    pages: HashMap<u32, u64>,
}

impl Spill {
    fn put(&mut self, page_no: u32, page: &[u8]) -> std::io::Result<()> {
        let slot = self.free.pop().unwrap_or_else(|| {
            self.slots += 1;
            self.slots - 1
        });
        self.file.write_all_at(page, slot * self.page_size)?;
        self.pages.insert(page_no, slot);
        Ok(())
    }

    fn take(&mut self, page_no: u32, buf: &mut [u8]) -> std::io::Result<bool> {
        let Some(slot) = self.pages.remove(&page_no) else {
            return Ok(false);
        };
        self.file.read_exact_at(buf, slot * self.page_size)?;
        self.free.push(slot);
        Ok(true)
    }
}

/// Reads an SQLite database from `input` and calls `on_row(rowid, record)` for every row of every table, in file
/// order. `spill` is a temporary file for pages that arrive before the row pointing to them (it is truncated, and
/// can be removed afterwards).
pub fn scan<R: Read>(
    mut input: R,
    spill: &Path,
    mut on_row: impl FnMut(i64, &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>>,
) -> Result<ScanStats, SqliteError> {
    let mut header = [0u8; 100];
    input.read_exact(&mut header).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            SqliteError::NotSqlite
        } else {
            e.into()
        }
    })?;
    if header.get(..16) != Some(HEADER_MAGIC.as_slice()) {
        return Err(SqliteError::NotSqlite);
    }
    let page_size = match be_u16(&header, 16) {
        Some(1) => 65536,
        Some(n) if n >= 512 && n.is_power_of_two() => n,
        _ => return Err(SqliteError::Malformed("bad page size".to_owned())),
    };
    let reserved = usize::from(header[20]);
    let usable = page_size
        .checked_sub(reserved)
        .filter(|u| *u >= 480)
        .ok_or_else(|| SqliteError::Malformed("bad reserved space".to_owned()))?;
    let page_count = be_u32(&header, 28).unwrap_or(0);
    if be_u32(&header, 32).unwrap_or(0) != 0 || be_u32(&header, 36).unwrap_or(0) != 0 {
        return Err(SqliteError::Unsupported(
            "the database has free pages (a dump made with VACUUM INTO has none)".to_owned(),
        ));
    }
    if be_u32(&header, 52).unwrap_or(0) != 0 {
        return Err(SqliteError::Unsupported("auto-vacuum".to_owned()));
    }
    if be_u32(&header, 56).unwrap_or(1) != 1 {
        return Err(SqliteError::Unsupported(
            "text encodings other than UTF-8".to_owned(),
        ));
    }
    // An overflow page starts with the number of the next one: below this, it can't start with a table leaf's type.
    if page_count >= u32::from(TABLE_LEAF) << 24 {
        return Err(SqliteError::Unsupported(format!("{page_count} pages")));
    }

    let spill_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(spill)?;
    let mut state = Scanner {
        usable,
        max_local_leaf: usable - 35,
        min_local: (usable - 12) * 32 / 255 - 23,
        waiting: HashMap::new(),
        spill: Spill {
            file: spill_file,
            page_size: page_size as u64,
            slots: 0,
            free: Vec::new(),
            pages: HashMap::new(),
        },
        stats: ScanStats::default(),
        ambiguous_index_interior: page_count >= u32::from(INDEX_INTERIOR) << 24,
    };

    let mut page = vec![0u8; page_size];
    if let (Some(dst), Some(src)) = (page.get_mut(..100), header.get(..100)) {
        dst.copy_from_slice(src);
    }
    let mut filled = 100;
    let mut page_no: u32 = 1;
    let mut chased = vec![0u8; page_size];
    loop {
        if let Some(rest) = page.get_mut(filled..) {
            match read_full(&mut input, rest)? {
                n if n == rest.len() => {}
                0 if filled == 0 => break,
                _ => return Err(SqliteError::Malformed("the file ends mid-page".to_owned())),
            }
        }
        filled = 0;
        state.stats.pages += 1;
        state.page(page_no, &page, &mut chased, &mut on_row)?;
        page_no = page_no
            .checked_add(1)
            .ok_or_else(|| SqliteError::Malformed("too many pages".to_owned()))?;
    }

    if let Some(pending) = state.waiting.values().next() {
        return Err(SqliteError::Malformed(format!(
            "{} rows (e.g. rowid {}) point to overflow pages that never came",
            state.waiting.len(),
            pending.rowid
        )));
    }
    Ok(state.stats)
}

/// Reads until `buf` is full or the input ends; returns how much was read.
fn read_full<R: Read>(input: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match input.read(buf.get_mut(n..).unwrap_or_default()) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

type RowFn<'a> = dyn FnMut(i64, &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> + 'a;

struct Scanner {
    usable: usize,
    max_local_leaf: usize,
    min_local: usize,
    /// Rows waiting for an overflow page, by that page's number.
    waiting: HashMap<u32, Pending>,
    spill: Spill,
    stats: ScanStats,
    /// Whether an overflow page can look like an index interior page (`0x02`): the file has pages numbered
    /// `0x02000000` and up.
    ambiguous_index_interior: bool,
}

impl Scanner {
    fn page(
        &mut self,
        page_no: u32,
        page: &[u8],
        chased: &mut [u8],
        on_row: &mut RowFn<'_>,
    ) -> Result<(), SqliteError> {
        if let Some(pending) = self.waiting.remove(&page_no) {
            return self.overflow(pending, page, chased, on_row);
        }
        let off = if page_no == 1 { 100 } else { 0 };
        match page.get(off).copied() {
            Some(TABLE_LEAF) => self.leaf(page_no, page, off, chased, on_row),
            Some(TABLE_INTERIOR | INDEX_LEAF) => Ok(()),
            Some(INDEX_INTERIOR) if !self.ambiguous_index_interior => Ok(()),
            _ => {
                // An overflow page whose row hasn't come yet, or a page of no interest that can't be told from one.
                self.spill.put(page_no, page)?;
                self.stats.max_early_pages = self.stats.max_early_pages.max(self.spill.pages.len());
                Ok(())
            }
        }
    }

    /// How much of a payload of `len` bytes is stored on the leaf page itself.
    fn local_len(&self, len: usize) -> usize {
        if len <= self.max_local_leaf {
            return len;
        }
        let k = self.min_local + (len - self.min_local) % (self.usable - 4);
        if k <= self.max_local_leaf {
            k
        } else {
            self.min_local
        }
    }

    fn leaf(
        &mut self,
        page_no: u32,
        page: &[u8],
        off: usize,
        chased: &mut [u8],
        on_row: &mut RowFn<'_>,
    ) -> Result<(), SqliteError> {
        let bad = |what: &str| SqliteError::Malformed(format!("table leaf page {page_no}: {what}"));
        let cells = be_u16(page, off + 3).ok_or_else(|| bad("header"))?;
        let pointers = off + 8;
        let pointers_end = pointers + 2 * cells;
        if pointers_end > self.usable {
            return Err(bad("too many cells"));
        }
        for i in 0..cells {
            let at = be_u16(page, pointers + 2 * i).ok_or_else(|| bad("cell pointer"))?;
            if at < pointers_end || at >= self.usable {
                return Err(bad("cell pointer out of range"));
            }
            let cell = page.get(at..self.usable).ok_or_else(|| bad("cell"))?;
            let (len, n1) = varint(cell).ok_or_else(|| bad("payload size"))?;
            let (rowid, n2) =
                varint(cell.get(n1..).unwrap_or_default()).ok_or_else(|| bad("rowid"))?;
            let len = usize::try_from(len).map_err(|_| bad("payload size"))?;
            let rowid = rowid.cast_signed();
            let local = self.local_len(len);
            let start = n1 + n2;
            let payload = cell
                .get(start..start + local)
                .ok_or_else(|| bad("payload"))?;
            if local == len {
                self.stats.rows += 1;
                on_row(rowid, payload).map_err(SqliteError::Row)?;
                continue;
            }
            let first = be_u32(cell, start + local).ok_or_else(|| bad("overflow pointer"))?;
            let mut data = Vec::with_capacity(len);
            data.extend_from_slice(payload);
            let pending = Pending {
                rowid,
                total: len,
                data,
            };
            self.chase(pending, first, chased, on_row)?;
        }
        Ok(())
    }

    /// Continues a row with the overflow page `page`.
    fn overflow(
        &mut self,
        mut pending: Pending,
        page: &[u8],
        chased: &mut [u8],
        on_row: &mut RowFn<'_>,
    ) -> Result<(), SqliteError> {
        let next = self.append(&mut pending, page)?;
        self.chase(pending, next, chased, on_row)
    }

    /// Adds an overflow page's content to a row; returns the next overflow page.
    fn append(&self, pending: &mut Pending, page: &[u8]) -> Result<u32, SqliteError> {
        let next = be_u32(page, 0).unwrap_or(0);
        let take = (pending.total - pending.data.len()).min(self.usable - 4);
        let content = page
            .get(4..4 + take)
            .ok_or_else(|| SqliteError::Malformed("short overflow page".to_owned()))?;
        pending.data.extend_from_slice(content);
        Ok(next)
    }

    /// Follows a row's overflow chain from page `next` through the pages set aside, until the row is complete (and
    /// handed to `on_row`) or the chain reaches a page not seen yet (and the row waits for it).
    fn chase(
        &mut self,
        mut pending: Pending,
        mut next: u32,
        chased: &mut [u8],
        on_row: &mut RowFn<'_>,
    ) -> Result<(), SqliteError> {
        loop {
            if pending.data.len() >= pending.total {
                self.stats.rows += 1;
                return on_row(pending.rowid, &pending.data).map_err(SqliteError::Row);
            }
            if next == 0 {
                return Err(SqliteError::Malformed(format!(
                    "the overflow chain of rowid {} ends early",
                    pending.rowid
                )));
            }
            if !self.spill.take(next, chased)? {
                if self.waiting.insert(next, pending).is_some() {
                    return Err(SqliteError::Malformed(format!(
                        "two rows share overflow page {next}"
                    )));
                }
                return Ok(());
            }
            next = self.append(&mut pending, chased)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_bytes(setup: &str, page_size: u32) -> Vec<u8> {
        let dir = std::env::temp_dir().join(format!(
            "lrclib-mirror-sqlite-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.sqlite3");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(&format!("PRAGMA page_size = {page_size};"))
            .unwrap();
        conn.execute_batch(setup).unwrap();
        conn.execute_batch("VACUUM").unwrap();
        drop(conn);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        bytes
    }

    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    fn rows_of(bytes: &[u8]) -> Result<Vec<(i64, Vec<Value<'static>>)>, SqliteError> {
        let spill = std::env::temp_dir().join(format!(
            "lrclib-mirror-spill-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        let mut rows = Vec::new();
        let result = scan(bytes, &spill, |rowid, payload| {
            let values = decode_record(payload)?
                .into_iter()
                .map(|v| match v {
                    Value::Text(t) => Value::Text(Vec::leak(t.to_vec())),
                    Value::Blob(t) => Value::Blob(Vec::leak(t.to_vec())),
                    Value::Null => Value::Null,
                    Value::Int(i) => Value::Int(i),
                    Value::Real(r) => Value::Real(r),
                })
                .collect();
            rows.push((rowid, values));
            Ok(())
        });
        let _ = std::fs::remove_file(&spill);
        result.map(|_| rows)
    }

    #[test]
    fn reads_every_row_with_overflow() {
        use std::fmt::Write as _;
        // Rows of every size around the overflow thresholds, values of every type.
        let mut setup = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b, c REAL);");
        for i in 1..=600 {
            let len = (i * 37) % 9000;
            let b = i64::from(i) * 1_000_003 - 300_000_000;
            let _ = write!(
                setup,
                "INSERT INTO t VALUES ({i}, printf('%.*c', {len}, 'x') || '{i}', {b}, {i}.5);"
            );
        }
        setup.push_str("INSERT INTO t VALUES (1000, NULL, x'00ff', NULL);");
        setup.push_str("CREATE INDEX t_a ON t (a);");
        for page_size in [512, 4096] {
            let rows = rows_of(&db_bytes(&setup, page_size)).unwrap();
            let t: Vec<_> = rows.iter().filter(|(_, v)| v.len() == 4).collect();
            assert_eq!(t.len(), 601, "page size {page_size}");
            for (rowid, v) in &t {
                if *rowid == 1000 {
                    assert_eq!(v[1], Value::Null);
                    assert_eq!(v[2], Value::Blob(&[0x00, 0xff]));
                    continue;
                }
                let len = usize::try_from((rowid * 37) % 9000).unwrap();
                let Value::Text(a) = v[1] else { panic!() };
                assert_eq!(a.len(), len + rowid.to_string().len());
                assert_eq!(v[2], Value::Int(rowid * 1_000_003 - 300_000_000));
                assert_eq!(
                    v[3],
                    Value::Real(f64::from(i32::try_from(*rowid).unwrap()) + 0.5)
                );
            }
        }
    }

    #[test]
    fn rejects_other_files_and_free_pages() {
        assert!(matches!(rows_of(b"hello"), Err(SqliteError::NotSqlite)));
        let mut bytes = db_bytes("CREATE TABLE t (a); INSERT INTO t VALUES (1);", 4096);
        bytes[36..40].copy_from_slice(&1u32.to_be_bytes());
        assert!(matches!(rows_of(&bytes), Err(SqliteError::Unsupported(_))));
        let bytes = db_bytes("CREATE TABLE t (a); INSERT INTO t VALUES (1);", 4096);
        assert!(matches!(
            rows_of(&bytes[..bytes.len() - 10]),
            Err(SqliteError::Malformed(_))
        ));
    }

    #[test]
    fn varints_and_integers() {
        assert_eq!(varint(&[0x7f]), Some((127, 1)));
        assert_eq!(varint(&[0x81, 0x00]), Some((128, 2)));
        assert_eq!(varint(&[0xff; 9]), Some((u64::MAX, 9)));
        assert_eq!(be_int(&[0xff]), -1);
        assert_eq!(be_int(&[0x01, 0x00]), 256);
        assert_eq!(be_int(&[0x80, 0, 0, 0, 0, 0]), -(1 << 47));
    }
}
