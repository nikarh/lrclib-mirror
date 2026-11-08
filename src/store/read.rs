//! Reading a store: tracks by id or by normalized name and artist, and their lyrics.
//!
//! Reads allocate little: a block is read and decompressed into buffers that each thread reuses ([`Scratch`]), and
//! tracks and lyrics are views borrowing from them. Only lyrics computed back from their stored form are new strings.

use std::borrow::Cow;
use std::fs::File;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};

use crate::lyricsfile;
use crate::search::Search;
use crate::text::{normalize, plain_from_lrc, synced_from_skeleton};

use super::{
    FORMAT_VERSION, IDS, L_INSTRUMENTAL, L_LF_SHIFT, L_PLAIN_SHIFT, L_SYNCED_SHIFT, L_WORD_SYNC,
    LYRICS, LYRICS_DICT, LYRICS_INDEX, META, Meta, Reader, SEARCH, T_ALBUM, T_ALBUM_LOWER,
    T_ARTIST, T_ARTIST_LOWER, T_BROKEN, T_DURATION, T_NAME, T_NAME_LOWER, TRACKS, TRACKS_DICT,
    TRACKS_INDEX, mode, sort_key, track_meta,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("{0}: {1}")]
    Corrupt(PathBuf, String),
    #[error("{0}: format {1}, this version reads format {FORMAT_VERSION}")]
    Format(PathBuf, u32),
    #[error("{0}: built by a version that normalizes names or builds lyrics differently")]
    Derivations(PathBuf),
    #[error("search index: {0}")]
    Search(String),
}

/// A track as LRCLIB's API answers it, borrowed from a decompressed block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Track<'a> {
    pub id: u32,
    pub name: Option<&'a str>,
    pub artist: Option<&'a str>,
    pub album: Option<&'a str>,
    pub duration: Option<f64>,
    /// LRCLIB's server can't read the track (or its lyrics) and answers with an error.
    pub broken: bool,
    /// The normalized name, artist and album the dump has.
    lower: [Lower<'a>; 3],
}

/// A normalized name of a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lower<'a> {
    /// What normalizing the name gives.
    Normalized,
    /// Something else, stored.
    Stored(Option<&'a str>),
}

impl Track<'_> {
    /// Whether the normalized name (0), artist (1) or album (2) the dump has for the track is `expected`.
    fn lower_is(&self, i: usize, expected: &str) -> bool {
        match self.lower.get(i) {
            Some(Lower::Stored(stored)) => *stored == Some(expected),
            _ => [self.name, self.artist, self.album]
                .get(i)
                .copied()
                .flatten()
                .is_some_and(|s| normalize(s) == expected),
        }
    }
}

/// A track's current lyrics, as LRCLIB's API answers them: borrowed when stored as they are.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lyrics<'a> {
    pub plain: Option<Cow<'a, str>>,
    pub synced: Option<Cow<'a, str>>,
    pub lyricsfile: Option<Cow<'a, str>>,
    pub instrumental: bool,
    pub word_sync: bool,
}

/// Buffers for reading one block: compressed and decompressed, and the zstd context. Reused from one request to
/// the next.
pub struct BlockBuf {
    compressed: Vec<u8>,
    raw: Vec<u8>,
    dctx: zstd::zstd_safe::DCtx<'static>,
}

impl Default for BlockBuf {
    fn default() -> Self {
        Self {
            compressed: Vec::new(),
            raw: Vec::new(),
            dctx: zstd::zstd_safe::DCtx::create(),
        }
    }
}

/// Per-thread buffers for reading: a track block and a lyrics block (both borrowed by a response).
#[derive(Default)]
pub struct Scratch {
    pub tracks: BlockBuf,
    pub lyrics: BlockBuf,
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::new(Scratch::default());
}

/// Runs `f` with this thread's [`Scratch`].
pub fn with_scratch<T>(f: impl FnOnce(&mut Scratch) -> T) -> T {
    SCRATCH.with(|s| match s.try_borrow_mut() {
        Ok(mut s) => f(&mut s),
        // Re-entered (not done by this crate): use a fresh one.
        Err(_) => f(&mut Scratch::default()),
    })
}

struct TrackBlock {
    key: u64,
    ordinal: u32,
    offset: u64,
}

struct LyricsBlock {
    ordinal: u32,
    offset: u64,
}

pub struct Store {
    dir: PathBuf,
    pub meta: Meta,
    tracks: File,
    lyrics: File,
    ids: File,
    /// One per block, and a sentinel.
    track_blocks: Vec<TrackBlock>,
    lyrics_blocks: Vec<LyricsBlock>,
    tracks_dict: zstd::dict::DecoderDictionary<'static>,
    lyrics_dict: zstd::dict::DecoderDictionary<'static>,
    pub search: Search,
}

fn le_u32(r: &mut Reader<'_>) -> u32 {
    r.bytes(4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .unwrap_or_default()
}

impl Store {
    /// Opens a finished store (one with `meta.json`).
    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        let path = |name: &str| dir.join(name);
        let read =
            |name: &str| std::fs::read(path(name)).map_err(|e| StoreError::Io(path(name), e));
        let open = |name: &str| File::open(path(name)).map_err(|e| StoreError::Io(path(name), e));
        let corrupt = |name: &str, what: &str| StoreError::Corrupt(path(name), what.to_owned());

        let meta: Meta =
            serde_json::from_slice(&read(META)?).map_err(|e| corrupt(META, &e.to_string()))?;
        if meta.format != FORMAT_VERSION {
            return Err(StoreError::Format(dir.to_owned(), meta.format));
        }
        if meta.derivations != super::derivations() {
            return Err(StoreError::Derivations(dir.to_owned()));
        }
        let track_blocks: Vec<TrackBlock> = read(TRACKS_INDEX)?
            .as_chunks::<20>()
            .0
            .iter()
            .map(|c| {
                let mut r = Reader::new(c);
                TrackBlock {
                    key: r.u64_le().unwrap_or_default(),
                    ordinal: le_u32(&mut r),
                    offset: r.u64_le().unwrap_or_default(),
                }
            })
            .collect();
        let lyrics_blocks: Vec<LyricsBlock> = read(LYRICS_INDEX)?
            .as_chunks::<12>()
            .0
            .iter()
            .map(|c| {
                let mut r = Reader::new(c);
                LyricsBlock {
                    ordinal: le_u32(&mut r),
                    offset: r.u64_le().unwrap_or_default(),
                }
            })
            .collect();
        if track_blocks.len() != meta.track_blocks as usize + 1
            || lyrics_blocks.len() != meta.lyrics_blocks as usize + 1
        {
            return Err(corrupt(TRACKS_INDEX, "block count"));
        }
        let search = Search::open(&path(SEARCH)).map_err(|e| StoreError::Search(e.to_string()))?;
        Ok(Self {
            dir: dir.to_owned(),
            tracks: open(TRACKS)?,
            lyrics: open(LYRICS)?,
            ids: open(IDS)?,
            track_blocks,
            lyrics_blocks,
            tracks_dict: zstd::dict::DecoderDictionary::copy(&read(TRACKS_DICT)?),
            lyrics_dict: zstd::dict::DecoderDictionary::copy(&read(LYRICS_DICT)?),
            meta,
            search,
        })
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn corrupt(&self, file: &str, what: &str) -> StoreError {
        StoreError::Corrupt(self.dir.join(file), what.to_owned())
    }

    /// The ordinal of the track with this id.
    pub fn ordinal(&self, id: i64) -> Result<Option<u32>, StoreError> {
        let Ok(id) = u32::try_from(id) else {
            return Ok(None);
        };
        if id > self.meta.max_track_id {
            return Ok(None);
        }
        let mut buf = [0u8; 4];
        self.ids
            .read_exact_at(&mut buf, u64::from(id) * 4)
            .map_err(|e| StoreError::Io(self.dir.join(IDS), e))?;
        Ok(u32::from_le_bytes(buf).checked_sub(1))
    }

    /// Reads and decompresses `[from, to)` of `file` into `buf.raw`.
    fn block(
        &self,
        file: &File,
        name: &str,
        ddict: &zstd::dict::DecoderDictionary<'static>,
        from: u64,
        to: u64,
        buf: &mut BlockBuf,
    ) -> Result<(), StoreError> {
        let BlockBuf {
            compressed,
            raw: out,
            dctx,
        } = buf;
        let len =
            usize::try_from(to.saturating_sub(from)).map_err(|_| self.corrupt(name, "offset"))?;
        compressed.resize(len, 0);
        file.read_exact_at(compressed, from)
            .map_err(|e| StoreError::Io(self.dir.join(name), e))?;
        let size = zstd::zstd_safe::get_frame_content_size(compressed)
            .ok()
            .flatten()
            .and_then(|s| usize::try_from(s).ok())
            .ok_or_else(|| self.corrupt(name, "block size"))?;
        out.clear();
        out.reserve(size);
        dctx.decompress_using_ddict(out, compressed, ddict.as_ddict())
            .map_err(|code| self.corrupt(name, zstd::zstd_safe::get_error_name(code)))?;
        Ok(())
    }

    /// Decompresses track block `b` into `buf`.
    fn load_track_block(&self, b: usize, buf: &mut BlockBuf) -> Result<(), StoreError> {
        let (Some(block), Some(next)) = (self.track_blocks.get(b), self.track_blocks.get(b + 1))
        else {
            return Err(self.corrupt(TRACKS_INDEX, "block"));
        };
        self.block(
            &self.tracks,
            TRACKS,
            &self.tracks_dict,
            block.offset,
            next.offset,
            buf,
        )
    }

    /// The track with this ordinal, borrowed from `buf`.
    pub fn track<'s>(&self, ordinal: u32, buf: &'s mut BlockBuf) -> Result<Track<'s>, StoreError> {
        let b = self
            .track_blocks
            .partition_point(|x| x.ordinal <= ordinal)
            .saturating_sub(1);
        let first = self.track_blocks.get(b).map_or(0, |x| x.ordinal);
        self.load_track_block(b, buf)?;
        let block = TrackBlockView::new(&buf.raw).ok_or_else(|| self.corrupt(TRACKS, "block"))?;
        block
            .records()
            .nth((ordinal - first) as usize)
            .flatten()
            .ok_or_else(|| self.corrupt(TRACKS, "ordinal"))
    }

    /// The lyrics of the track with this ordinal, borrowed from `buf` where stored as they are.
    pub fn lyrics<'s>(
        &self,
        ordinal: u32,
        track: &Track<'_>,
        buf: &'s mut BlockBuf,
    ) -> Result<Lyrics<'s>, StoreError> {
        let b = self
            .lyrics_blocks
            .partition_point(|x| x.ordinal <= ordinal)
            .saturating_sub(1);
        let (Some(block), Some(next)) = (self.lyrics_blocks.get(b), self.lyrics_blocks.get(b + 1))
        else {
            return Err(self.corrupt(LYRICS_INDEX, "block"));
        };
        self.block(
            &self.lyrics,
            LYRICS,
            &self.lyrics_dict,
            block.offset,
            next.offset,
            buf,
        )?;
        let mut r = Reader::new(&buf.raw);
        let corrupt = || self.corrupt(LYRICS, "entry");
        for _ in block.ordinal..ordinal {
            let n = usize::try_from(r.varint().ok_or_else(corrupt)?).map_err(|_| corrupt())?;
            r.bytes(n).ok_or_else(corrupt)?;
        }
        let n = usize::try_from(r.varint().ok_or_else(corrupt)?).map_err(|_| corrupt())?;
        decode_lyrics(r.bytes(n).ok_or_else(corrupt)?, track).ok_or_else(corrupt)
    }

    /// The ordinal of the track LRCLIB's `GET /api/get` finds: the one with the lowest id among those with this
    /// normalized name and artist, and album and duration (±2 s) when given.
    pub fn find(
        &self,
        name: &str,
        artist: &str,
        album: Option<&str>,
        duration: Option<f64>,
        buf: &mut BlockBuf,
    ) -> Result<Option<u32>, StoreError> {
        let key = sort_key(Some(artist), Some(name));
        let blocks = self.track_blocks.len().saturating_sub(1);
        // The first block that can hold the key: the last one starting before it (tracks with the key may begin at
        // its end), else the first.
        let mut b = self
            .track_blocks
            .get(..blocks)
            .unwrap_or_default()
            .partition_point(|x| x.key < key)
            .saturating_sub(1);
        let mut best: Option<(u32, u32)> = None; // (id, ordinal)
        while b < blocks && self.track_blocks.get(b).is_some_and(|x| x.key <= key) {
            let first = self.track_blocks.get(b).map_or(0, |x| x.ordinal);
            self.load_track_block(b, buf)?;
            let block =
                TrackBlockView::new(&buf.raw).ok_or_else(|| self.corrupt(TRACKS, "block"))?;
            // Only the records with the key are decoded.
            let Some(start) = (0..block.len()).find(|&i| block.key(i) >= key) else {
                b += 1;
                continue;
            };
            let mut past = false;
            for (i, record) in block.records().enumerate().skip(start) {
                if block.key(i) > key {
                    past = true;
                    break;
                }
                let track = record.ok_or_else(|| self.corrupt(TRACKS, "record"))?;
                if best.is_some_and(|(id, _)| id <= track.id) {
                    continue;
                }
                let matches = track.lower_is(0, name)
                    && track.lower_is(1, artist)
                    && album.is_none_or(|a| track.lower_is(2, a))
                    && duration.is_none_or(|d| {
                        track.duration.is_some_and(|t| t >= d - 2.0 && t <= d + 2.0)
                    });
                if matches {
                    #[allow(clippy::cast_possible_truncation)] // < TRACKS_PER_BLOCK
                    let ordinal = first + i as u32;
                    best = Some((track.id, ordinal));
                }
            }
            if past {
                break;
            }
            b += 1;
        }
        Ok(best.map(|(_, ordinal)| ordinal))
    }
}

/// A decompressed track block: `[count][keys, u64 LE][records]`.
struct TrackBlockView<'a> {
    n: usize,
    keys: &'a [u8],
    records: &'a [u8],
}

impl<'a> TrackBlockView<'a> {
    fn new(raw: &'a [u8]) -> Option<Self> {
        let mut r = Reader::new(raw);
        let n = usize::try_from(r.varint()?).ok()?;
        let keys = r.bytes(n.checked_mul(8)?)?;
        Some(Self {
            n,
            keys,
            records: raw.get(r.at..)?,
        })
    }

    fn len(&self) -> usize {
        self.n
    }

    fn key(&self, i: usize) -> u64 {
        self.keys
            .get(i * 8..i * 8 + 8)
            .and_then(|b| b.try_into().ok())
            .map_or(u64::MAX, u64::from_le_bytes)
    }

    /// The records in order (`None` for a corrupt one, which ends the iteration after it).
    fn records(&self) -> impl Iterator<Item = Option<Track<'a>>> + use<'a> {
        let mut r = Reader::new(self.records);
        let mut left = self.n;
        std::iter::from_fn(move || {
            if left == 0 {
                return None;
            }
            left -= 1;
            let track = parse_track(&mut r);
            if track.is_none() {
                left = 0;
            }
            Some(track)
        })
    }
}

fn parse_track<'a>(r: &mut Reader<'a>) -> Option<Track<'a>> {
    let id = u32::try_from(r.varint()?).ok()?;
    let flags = r.u8()?;
    let mut text = |bit: u8| -> Option<Option<&'a str>> {
        if flags & bit == 0 {
            return Some(None);
        }
        Some(Some(r.string()?))
    };
    let name = text(T_NAME)?;
    let artist = text(T_ARTIST)?;
    let album = text(T_ALBUM)?;
    let mut lower = [Lower::Normalized; 3];
    for (slot, bit) in lower
        .iter_mut()
        .zip([T_NAME_LOWER, T_ARTIST_LOWER, T_ALBUM_LOWER])
    {
        if flags & bit != 0 {
            *slot = Lower::Stored(match r.u8()? {
                0 => None,
                _ => Some(r.string()?),
            });
        }
    }
    let duration = if flags & T_DURATION == 0 {
        None
    } else {
        Some(f64::from_le_bytes(r.bytes(8)?.try_into().ok()?))
    };
    Some(Track {
        id,
        name,
        artist,
        album,
        duration,
        broken: flags & T_BROKEN != 0,
        lower,
    })
}

/// Plain and synced lyrics from their stored forms.
pub(super) fn lyrics_texts<'a>(
    plain_mode: u32,
    synced_mode: u32,
    plain: Option<&'a str>,
    synced: Option<&'a str>,
) -> (Option<Cow<'a, str>>, Option<Cow<'a, str>>) {
    let synced: Option<Cow<'a, str>> = match synced_mode {
        mode::TEXT => synced.map(Cow::Borrowed),
        mode::DERIVED => {
            synced.map(|s| Cow::Owned(synced_from_skeleton(s, plain.unwrap_or_default())))
        }
        _ => None,
    };
    let plain = match plain_mode {
        mode::TEXT => plain.map(Cow::Borrowed),
        mode::DERIVED => synced.as_deref().and_then(plain_from_lrc).map(Cow::Owned),
        _ => None,
    };
    (plain, synced)
}

fn decode_lyrics<'a>(entry: &'a [u8], track: &Track<'_>) -> Option<Lyrics<'a>> {
    let mut r = Reader::new(entry);
    let flags = u32::try_from(r.varint()?).ok()?;
    let plain_mode = (flags >> L_PLAIN_SHIFT) & 3;
    let synced_mode = (flags >> L_SYNCED_SHIFT) & 3;
    let lf_mode = (flags >> L_LF_SHIFT) & 3;
    let instrumental = flags & L_INSTRUMENTAL != 0;
    let plain = if plain_mode == mode::TEXT {
        Some(r.string()?)
    } else {
        None
    };
    let synced = if synced_mode == mode::NULL {
        None
    } else {
        Some(r.string()?)
    };
    let stored_lf = if matches!(lf_mode, mode::TEXT | mode::HEAD) {
        Some(r.string()?)
    } else {
        None
    };
    let (plain, synced) = lyrics_texts(plain_mode, synced_mode, plain, synced);
    let lyricsfile = match lf_mode {
        mode::TEXT => stored_lf.map(Cow::Borrowed),
        mode::DERIVED => {
            let meta = track_meta(track.name, track.artist, track.album, track.duration);
            let mut lf = lyricsfile::head(&meta, instrumental)?;
            lf.push_str(&lyricsfile::body(
                plain.as_deref(),
                synced.as_deref(),
                instrumental,
            )?);
            Some(Cow::Owned(lf))
        }
        mode::HEAD => {
            let mut lf = stored_lf?.to_owned();
            lf.push_str(&lyricsfile::body(
                plain.as_deref(),
                synced.as_deref(),
                instrumental,
            )?);
            Some(Cow::Owned(lf))
        }
        _ => None,
    };
    Some(Lyrics {
        plain,
        synced,
        lyricsfile,
        instrumental,
        word_sync: flags & L_WORD_SYNC != 0,
    })
}
