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
//! - Rows waiting for overflow pages that come later are kept in memory up to [`WAITING_MEMORY`], and beyond that
//!   in the spill file too: in some dumps a table's leaves all come long before their overflow pages, and the rows
//!   waiting for them add up to tens of GB. Rows wait there compressed.
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
/// What rows waiting for overflow pages may take in memory; the rest wait in the spill file.
const WAITING_MEMORY: usize = 64 << 20;

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
    /// Rows that waited for an overflow page not seen yet (at most this many at once).
    pub max_waiting_rows: usize,
    /// The most the spill file held at once (compressed).
    pub max_spill_bytes: u64,
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

/// A row whose record continues in overflow pages: what it has so far is in parts set aside in the spill file
/// ([`Spill::write`]), then in memory.
struct Pending {
    rowid: i64,
    total: usize,
    spilled: Box<[u32]>,
    spilled_len: usize,
    data: Vec<u8>,
}

impl Pending {
    fn len(&self) -> usize {
        self.spilled_len + self.data.len()
    }
}

/// A row waiting for an overflow page (kept small: there can be millions). A row that waits again is set aside only
/// as far as it grew since it last waited: a row whose overflow pages come one by one, long apart, isn't written
/// again and again.
struct Waiting {
    rowid: i64,
    total: u32,
    spilled_len: u32,
    held: Held,
}

enum Held {
    Memory(Vec<u8>),
    /// Its parts in the spill file.
    Spilled(Box<[u32]>),
}

/// In the length of a part set aside: written as it is, not compressed.
const STORED: u32 = 1 << 31;
/// The size of the units the spill file is made of.
const SPILL_UNIT: usize = 512;

/// Pages that arrived before the row (or overflow page) pointing to them, and the parts of rows waiting for overflow
/// pages that don't fit in memory, kept in a temporary file. Each is written to units of the file (not necessarily
/// adjacent), rows compressed. Units are reused once what was in them is claimed, so the file stays about as large as
/// the most set aside at once, compressed.
struct Spill {
    file: File,
    units: u32,
    free: Vec<u32>,
    /// How many bytes set aside are in the file, and the most there were.
    bytes: u64,
    max_bytes: u64,
    /// Pages set aside, by number ([`Spill::write`]).
    pages: HashMap<u32, Box<[u32]>>,
    compressor: zstd::bulk::Compressor<'static>,
    decompressor: zstd::bulk::Decompressor<'static>,
    buf: Vec<u8>,
}

impl Spill {
    fn new(file: File) -> std::io::Result<Self> {
        Ok(Self {
            file,
            units: 0,
            free: Vec::new(),
            bytes: 0,
            max_bytes: 0,
            pages: HashMap::new(),
            compressor: zstd::bulk::Compressor::new(1)?,
            decompressor: zstd::bulk::Decompressor::new()?,
            buf: Vec::new(),
        })
    }

    /// The runs of adjacent units of `units`: (first unit, index in `units`, count).
    fn runs(units: &[u32]) -> impl Iterator<Item = (u32, usize, usize)> + '_ {
        let mut i = 0;
        std::iter::from_fn(move || {
            let first = *units.get(i)?;
            let start = i;
            let mut next = first;
            i += 1;
            while next
                .checked_add(1)
                .is_some_and(|n| units.get(i) == Some(&n))
            {
                next += 1;
                i += 1;
            }
            Some((first, start, i - start))
        })
    }

    /// Sets `data` aside, compressed or not, and appends where it is to `parts`: its length, the length of what is
    /// written (with [`STORED`] when not compressed), and the units it is in.
    fn write(&mut self, data: &[u8], parts: &mut Vec<u32>, compress: bool) -> std::io::Result<()> {
        let too_large = || std::io::Error::other("too large to set aside");
        let raw = u32::try_from(data.len()).map_err(|_| too_large())?;
        let compressed = if compress {
            std::borrow::Cow::Owned(self.compressor.compress(data)?)
        } else {
            std::borrow::Cow::Borrowed(data)
        };
        let len = u32::try_from(compressed.len())
            .ok()
            .filter(|l| l & STORED == 0)
            .ok_or_else(too_large)?;
        parts.reserve_exact(2 + compressed.len().div_ceil(SPILL_UNIT));
        parts.push(raw);
        parts.push(if compress { len } else { len | STORED });
        let at = parts.len();
        for _ in 0..compressed.len().div_ceil(SPILL_UNIT) {
            let unit = if let Some(unit) = self.free.pop() {
                unit
            } else {
                self.units = self
                    .units
                    .checked_add(1)
                    .ok_or_else(|| std::io::Error::other("the spill file is full (2^32 units)"))?;
                self.units - 1
            };
            parts.push(unit);
        }
        let units = parts.get_mut(at..).unwrap_or_default();
        // Units taken from the end of the file are adjacent: written at once.
        units.sort_unstable();
        for (first, at, count) in Self::runs(units) {
            let from = at * SPILL_UNIT;
            let to = ((at + count) * SPILL_UNIT).min(compressed.len());
            self.file.write_all_at(
                compressed.get(from..to).unwrap_or_default(),
                u64::from(first) * SPILL_UNIT as u64,
            )?;
        }
        self.bytes += u64::from(len);
        self.max_bytes = self.max_bytes.max(self.bytes);
        Ok(())
    }

    /// The parts of `parts`: (length, compressed length, units).
    fn parts(mut parts: &[u32]) -> impl Iterator<Item = (u32, u32, &[u32])> {
        std::iter::from_fn(move || {
            let (&raw, rest) = parts.split_first()?;
            let (&len, rest) = rest.split_first()?;
            let (units, rest) =
                rest.split_at_checked(((len & !STORED) as usize).div_ceil(SPILL_UNIT))?;
            parts = rest;
            Some((raw, len, units))
        })
    }

    /// Reads back what was set aside into `out` (its length), and frees its units.
    fn read(
        &mut self,
        (raw, len, units): (u32, u32, &[u32]),
        out: &mut [u8],
    ) -> std::io::Result<()> {
        if out.len() != raw as usize {
            return Err(std::io::Error::other("spill: wrong length"));
        }
        let stored = len & STORED != 0;
        let len = len & !STORED;
        let mut buf = std::mem::take(&mut self.buf);
        let into: &mut [u8] = if stored {
            out
        } else {
            buf.resize(len as usize, 0);
            &mut buf
        };
        for (first, at, count) in Self::runs(units) {
            let from = at * SPILL_UNIT;
            let to = ((at + count) * SPILL_UNIT).min(into.len());
            self.file.read_exact_at(
                into.get_mut(from..to).unwrap_or_default(),
                u64::from(first) * SPILL_UNIT as u64,
            )?;
        }
        if !stored && self.decompressor.decompress_to_buffer(&buf, out)? != out.len() {
            return Err(std::io::Error::other("spill: short data"));
        }
        self.buf = buf;
        self.free.extend_from_slice(units);
        self.bytes -= u64::from(len);
        Ok(())
    }

    fn put(&mut self, page_no: u32, page: &[u8]) -> std::io::Result<()> {
        let mut parts = Vec::new();
        // Pages are claimed soon, most of them: not worth compressing.
        self.write(page, &mut parts, false)?;
        self.pages.insert(page_no, parts.into_boxed_slice());
        Ok(())
    }

    /// Reads a page set aside into `buf` (a page), if there is one.
    fn take(&mut self, page_no: u32, buf: &mut [u8]) -> std::io::Result<bool> {
        let Some(parts) = self.pages.remove(&page_no) else {
            return Ok(false);
        };
        for part in Self::parts(&parts) {
            self.read(part, buf)?;
        }
        Ok(true)
    }

    /// Appends what was set aside to `out`.
    fn read_to_end(&mut self, parts: &[u32], out: &mut Vec<u8>) -> std::io::Result<()> {
        for part in Self::parts(parts) {
            let start = out.len();
            out.resize(start + part.0 as usize, 0);
            self.read(part, out.get_mut(start..).unwrap_or_default())?;
        }
        Ok(())
    }
}

/// Reads an SQLite database from `input` and calls `on_row(rowid, record)` for every row of every table, in file
/// order. `spill` is a temporary file for pages that arrive before the row pointing to them (it is truncated, and
/// can be removed afterwards).
pub fn scan<R: Read>(
    input: R,
    spill: &Path,
    on_row: impl FnMut(i64, &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>>,
) -> Result<ScanStats, SqliteError> {
    scan_with(input, spill, WAITING_MEMORY, on_row)
}

/// [`scan`], with rows waiting for overflow pages taking at most `waiting_memory` bytes in memory.
fn scan_with<R: Read>(
    mut input: R,
    spill: &Path,
    waiting_memory: usize,
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
        waiting_memory: 0,
        max_waiting_memory: waiting_memory,
        spill: Spill::new(spill_file)?,
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
    state.stats.max_spill_bytes = state.spill.max_bytes;
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
    waiting: HashMap<u32, Waiting>,
    /// The memory the rows waiting in memory take, and what they may take.
    waiting_memory: usize,
    max_waiting_memory: usize,
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
        if let Some(waiting) = self.waiting.remove(&page_no) {
            let total = waiting.total as usize;
            let spilled_len = waiting.spilled_len as usize;
            let (spilled, data) = match waiting.held {
                Held::Memory(data) => {
                    self.waiting_memory -= data.capacity();
                    (Box::default(), data)
                }
                Held::Spilled(parts) => (parts, Vec::with_capacity(total - spilled_len)),
            };
            let pending = Pending {
                rowid: waiting.rowid,
                total,
                spilled,
                spilled_len,
                data,
            };
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
                spilled: Box::default(),
                spilled_len: 0,
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
        let take = (pending.total - pending.len()).min(self.usable - 4);
        let content = page
            .get(4..4 + take)
            .ok_or_else(|| SqliteError::Malformed("short overflow page".to_owned()))?;
        pending.data.extend_from_slice(content);
        Ok(next)
    }

    /// Sets a row aside until its overflow page `next` comes: in memory while there is room, else in the spill file.
    fn wait(&mut self, pending: Pending, next: u32) -> Result<(), SqliteError> {
        if self.waiting.contains_key(&next) {
            return Err(SqliteError::Malformed(format!(
                "two rows share overflow page {next}"
            )));
        }
        let too_large = || SqliteError::Malformed(format!("row {} is too large", pending.rowid));
        let total = u32::try_from(pending.total).map_err(|_| too_large())?;
        let mut spilled_len = pending.spilled_len;
        let held = if pending.spilled.is_empty()
            && self.waiting_memory + pending.data.capacity() <= self.max_waiting_memory
        {
            self.waiting_memory += pending.data.capacity();
            Held::Memory(pending.data)
        } else {
            let mut parts = pending.spilled.into_vec();
            if !pending.data.is_empty() {
                self.spill.write(&pending.data, &mut parts, true)?;
                spilled_len += pending.data.len();
            }
            Held::Spilled(parts.into_boxed_slice())
        };
        let spilled_len = u32::try_from(spilled_len).map_err(|_| too_large())?;
        self.waiting.insert(
            next,
            Waiting {
                rowid: pending.rowid,
                total,
                spilled_len,
                held,
            },
        );
        self.stats.max_waiting_rows = self.stats.max_waiting_rows.max(self.waiting.len());
        Ok(())
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
            if pending.len() >= pending.total {
                self.stats.rows += 1;
                if pending.spilled.is_empty() {
                    return on_row(pending.rowid, &pending.data).map_err(SqliteError::Row);
                }
                let mut row = Vec::with_capacity(pending.total);
                self.spill.read_to_end(&pending.spilled, &mut row)?;
                row.extend_from_slice(&pending.data);
                return on_row(pending.rowid, &row).map_err(SqliteError::Row);
            }
            if next == 0 {
                return Err(SqliteError::Malformed(format!(
                    "the overflow chain of rowid {} ends early",
                    pending.rowid
                )));
            }
            if !self.spill.take(next, chased)? {
                return self.wait(pending, next);
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

    type Rows = Vec<(i64, Vec<Value<'static>>)>;

    fn rows_of(bytes: &[u8]) -> Result<Rows, SqliteError> {
        rows_with(bytes, WAITING_MEMORY).map(|(rows, _)| rows)
    }

    fn rows_with(bytes: &[u8], waiting_memory: usize) -> Result<(Rows, ScanStats), SqliteError> {
        let spill = std::env::temp_dir().join(format!(
            "lrclib-mirror-spill-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        let mut rows = Vec::new();
        let result = scan_with(bytes, &spill, waiting_memory, |rowid, payload| {
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
        result.map(|stats| (rows, stats))
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
            let bytes = db_bytes(&setup, page_size);
            let rows = rows_of(&bytes).unwrap();
            // Rows waiting for their overflow pages in the spill file read the same as rows waiting in memory.
            let (spilled, stats) = rows_with(&bytes, 0).unwrap();
            assert!(stats.max_waiting_rows > 0, "page size {page_size}");
            assert_eq!(spilled, rows, "page size {page_size}");
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

    #[test]
    fn spill_reuses_claimed_units() {
        let path = std::env::temp_dir().join(format!(
            "lrclib-mirror-spill-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let mut spill = Spill::new(file).unwrap();
        // Incompressible data, so that it takes space.
        let mut x = 0x2545_f491_4f6c_dd1d_u64;
        let mut noise = |n: usize| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x.to_le_bytes()[0]
                })
                .collect()
        };
        let read = |spill: &mut Spill, s: &[u32]| {
            let mut out = b"before".to_vec();
            spill.read_to_end(s, &mut out).unwrap();
            assert_eq!(&out[..6], b"before");
            out.split_off(6)
        };
        let rows: Vec<Vec<u8>> = (0..100).map(|i| noise(100 + i * 97)).collect();
        // Each row set aside in two parts.
        let write = |spill: &mut Spill, r: &[u8]| {
            let mut parts = Vec::new();
            let (a, b) = r.split_at(r.len() / 3);
            spill.write(a, &mut parts, true).unwrap();
            spill.write(b, &mut parts, false).unwrap();
            assert_eq!(Spill::parts(&parts).count(), 2);
            parts
        };
        let mut held: Vec<Option<Vec<u32>>> =
            rows.iter().map(|r| Some(write(&mut spill, r))).collect();
        let page = noise(4096);
        spill.put(7, &page).unwrap();
        let peak = spill.units;
        let mut buf = vec![0u8; 4096];
        assert!(!spill.take(8, &mut buf).unwrap());
        assert!(spill.take(7, &mut buf).unwrap());
        assert_eq!(buf, page);
        // Every other row is claimed, and set aside again in units of others.
        for i in (0..rows.len()).step_by(2) {
            let s = held[i].take().unwrap();
            assert_eq!(read(&mut spill, &s), rows[i]);
        }
        for i in (0..rows.len()).step_by(2).rev() {
            held[i] = Some(write(&mut spill, &rows[i]));
        }
        assert!(spill.units <= peak, "{} > {peak}", spill.units);
        for (row, s) in rows.iter().zip(&held) {
            assert_eq!(&read(&mut spill, s.as_ref().unwrap()), row);
        }
        assert_eq!(spill.bytes, 0);
        assert!(spill.max_bytes > rows.iter().map(|r| r.len() as u64).sum::<u64>());
        assert!(std::fs::metadata(&path).unwrap().len() <= u64::from(peak) * SPILL_UNIT as u64);
        std::fs::remove_file(&path).unwrap();
    }
}
