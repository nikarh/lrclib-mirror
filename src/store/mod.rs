//! The store: a compact, read-only copy of what LRCLIB's API answers from (each track with its current lyrics), and
//! the search index. Built from a dump by [`import`], read by [`Store`].
//!
//! Files of a store directory:
//!
//! | File | |
//! | --- | --- |
//! | `meta.json` | format version, counts; written last, so a store without it is incomplete |
//! | `tracks.zst`, `tracks.idx` | track records in blocks of up to [`TRACKS_PER_BLOCK`], zstd-compressed with `tracks.dict`; the index has each block's first sort key, first ordinal and offset |
//! | `lyrics.zst`, `lyrics.idx` | lyrics in blocks of ~[`LYRICS_BLOCK_BYTES`], zstd-compressed with `lyrics.dict`; the index has each block's first ordinal and offset |
//! | `ids.bin` | for every track id, the track's ordinal + 1 (0: no such track), u32 little-endian |
//! | `search/` | the Tantivy index of track names, as LRCLIB's server builds it |
//!
//! Tracks are stored in the order of their sort key, `hash(artist_name_lower) << 32 | hash(name_lower)`, then by id:
//! the tracks a metadata lookup can match are next to each other, and so are an artist's lyrics, which repeat a lot
//! (the same song on several albums) and compress well together. A track's ordinal is its position in that order;
//! its lyrics are entry `ordinal` of the lyrics blocks.
//!
//! Lyrics are stored as LRCLIB's server answers them, but without what can be computed back exactly (checked for
//! every row when importing): plain lyrics that are the synced ones without timestamps, synced lyrics whose lines
//! are the plain ones (only the timestamps are kept), and Lyricsfiles that are what the server builds from the track
//! and its lyrics (or that differ only in their metadata). Names normalized for lookups are stored only when they
//! aren't what the normalization gives today.

mod import;
mod read;

pub use import::{ImportError, ImportOptions, ImportStats, import};
pub use read::{Scratch, Store, StoreError, with_scratch};

pub const FORMAT_VERSION: u32 = 1;
pub const TRACKS_PER_BLOCK: usize = 128;
pub const LYRICS_BLOCK_BYTES: usize = 64 << 10;

const META: &str = "meta.json";
const TRACKS: &str = "tracks.zst";
const TRACKS_INDEX: &str = "tracks.idx";
const TRACKS_DICT: &str = "tracks.dict";
const LYRICS: &str = "lyrics.zst";
const LYRICS_INDEX: &str = "lyrics.idx";
const LYRICS_DICT: &str = "lyrics.dict";
const IDS: &str = "ids.bin";
pub const SEARCH: &str = "search";

/// `meta.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    pub format: u32,
    /// [`derivations`] of the version that built the store.
    pub derivations: u64,
    pub tracks: u32,
    pub max_track_id: u32,
    pub track_blocks: u32,
    pub lyrics_blocks: u32,
    pub stats: ImportStats,
}

/// A fingerprint of the functions whose output the store leaves out and computes back: name normalization, plain
/// lyrics from synced ones, synced lyrics from skeletons, Lyricsfiles. Should a dependency change what they give, a
/// store built by another version no longer answers as the dump would, and is imported again.
#[must_use]
pub fn derivations() -> u64 {
    // Raised with every change to how the store leaves out and computes back text (the samples below catch changes
    // of dependencies, not necessarily of this code).
    const VERSION: u8 = 2;
    use crate::text::{normalize, plain_from_lrc, synced_from_skeleton, synced_skeleton};
    const NAMES: &[&str] = &[
        "Café del Mar (Remix)",
        "  Don't   STOP  me—now! ",
        "AC/DC",
        "Ærøskøbing Ⅻ ﬁ ß İstanbul",
        "Ünïcödé ‘quotes’ “double” «guillemets» – dash",
        "日本語のタイトル 中文 한국어",
        "Привет, мир! Ελληνικά",
        "tab\tnew\nline\0nul",
        "emoji 🎵 ♪ ★",
    ];
    const LRC: &str = "[ar: Someone]\n[00:01.00] One\n[00:02.5]Two: it's\n\nUntimed line\n[01:03.123][02:00.00] Twice\n\
                       [00:04.00]   - Quoted 'single' \"double\" #hash\n[00:05.00] key: value\n[au: x]";
    let mut out = String::new();
    for name in NAMES {
        out.push_str(&normalize(name));
        out.push('\0');
    }
    let plain = plain_from_lrc(LRC).unwrap_or_default();
    out.push_str(&plain);
    let skeleton = synced_skeleton(LRC, &plain).unwrap_or_default();
    out.push_str(&synced_from_skeleton(&skeleton, &plain));
    for (i, name) in NAMES.iter().enumerate() {
        let meta = crate::lyricsfile::Meta {
            title: name,
            artist: NAMES.get(i + 1).copied().unwrap_or_default(),
            album: if i % 2 == 0 { name } else { "" },
            #[allow(clippy::cast_precision_loss)]
            duration: i as f64 * 61.237,
        };
        for (plain, synced, instrumental) in [
            (None, Some(LRC), false),
            (Some(*name), None, false),
            (None, Some(LRC), true),
        ] {
            out.push_str(
                &crate::lyricsfile::build(&meta, plain, synced, instrumental).unwrap_or_default(),
            );
        }
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in std::iter::once(VERSION).chain(out.bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The sort key of a track: its normalized artist and name, hashed.
#[must_use]
pub fn sort_key(artist_lower: Option<&str>, name_lower: Option<&str>) -> u64 {
    (u64::from(hash32(artist_lower.unwrap_or_default())) << 32)
        | u64::from(hash32(name_lower.unwrap_or_default()))
}

/// FNV-1a, folded to 32 bits. Part of the format: it orders the tracks.
fn hash32(s: &str) -> u32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    #[allow(clippy::cast_possible_truncation)] // folding
    {
        (h ^ (h >> 32)) as u32
    }
}

/// The Lyricsfile metadata of a track, as LRCLIB builds it.
fn track_meta<'a>(
    name: Option<&'a str>,
    artist: Option<&'a str>,
    album: Option<&'a str>,
    duration: Option<f64>,
) -> crate::lyricsfile::Meta<'a> {
    crate::lyricsfile::Meta {
        title: name.unwrap_or_default(),
        artist: artist.unwrap_or_default(),
        album: album.unwrap_or_default(),
        duration: duration.unwrap_or_default(),
    }
}

// Track record flags.
const T_NAME: u8 = 1;
const T_ARTIST: u8 = 1 << 1;
const T_ALBUM: u8 = 1 << 2;
const T_DURATION: u8 = 1 << 3;
/// The stored normalized name differs from what normalizing the name gives; it follows the names.
const T_NAME_LOWER: u8 = 1 << 4;
const T_ARTIST_LOWER: u8 = 1 << 5;
const T_ALBUM_LOWER: u8 = 1 << 6;
/// LRCLIB's server can't read this track's row (a column of an unexpected type): it answers with an error.
const T_BROKEN: u8 = 1 << 7;

// Lyrics entry flags: 2 bits per text, then booleans.
const L_PLAIN_SHIFT: u32 = 0;
const L_SYNCED_SHIFT: u32 = 2;
const L_LF_SHIFT: u32 = 4;
const L_INSTRUMENTAL: u32 = 1 << 6;
const L_WORD_SYNC: u32 = 1 << 7;

/// How a text of a lyrics entry is stored.
mod mode {
    pub const NULL: u32 = 0;
    pub const TEXT: u32 = 1;
    /// Plain: from the synced lyrics. Synced: a skeleton over the plain lyrics. Lyricsfile: built from the track.
    pub const DERIVED: u32 = 2;
    /// Lyricsfile only: its metadata part is stored, the rest is built from the lyrics.
    pub const HEAD: u32 = 3;
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        #[allow(clippy::cast_possible_truncation)] // low 7 bits
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    #[allow(clippy::cast_possible_truncation)] // < 0x80
    out.push(v as u8);
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

/// A cursor over encoded bytes.
struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, at: 0 }
    }

    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.buf.get(self.at)?;
            self.at += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    fn u8(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.at)?;
        self.at += 1;
        Some(b)
    }

    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let b = self.buf.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(b)
    }

    fn string(&mut self) -> Option<&'a str> {
        let n = usize::try_from(self.varint()?).ok()?;
        std::str::from_utf8(self.bytes(n)?).ok()
    }

    fn u64_le(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints() {
        for v in [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            assert_eq!(Reader::new(&out).varint(), Some(v));
        }
    }

    #[test]
    fn sort_keys_group_artists() {
        let a = sort_key(Some("artist"), Some("one"));
        let b = sort_key(Some("artist"), Some("two"));
        assert_eq!(a >> 32, b >> 32);
        assert_ne!(a, b);
        // Part of the format: must not change.
        assert_eq!(hash32(""), 0xcbf2_9ce4 ^ 0x8422_2325);
    }
}
