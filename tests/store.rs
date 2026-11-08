//! Imports a small database with LRCLIB's schema through the streaming importer, and checks every track against
//! SQLite answering the queries LRCLIB's server makes.

// A test: panicking on the unexpected is the point.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::type_complexity
)]

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use lrclib_mirror::lyricsfile;
use lrclib_mirror::store::{ImportOptions, Store, import, with_scratch};
use lrclib_mirror::text::normalize;
use rusqlite::{Connection, OptionalExtension as _, params};

const SCHEMA: &str = "
CREATE TABLE tracks (
  id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, name_lower TEXT, artist_name TEXT, artist_name_lower TEXT,
  album_name TEXT, album_name_lower TEXT, duration FLOAT, last_lyrics_id INTEGER, created_at DATETIME,
  updated_at DATETIME, FOREIGN KEY (last_lyrics_id) REFERENCES lyrics (id),
  UNIQUE(name_lower, artist_name_lower, album_name_lower, duration)
);
CREATE TABLE lyrics (
  id INTEGER PRIMARY KEY AUTOINCREMENT, plain_lyrics TEXT, synced_lyrics TEXT, track_id INTEGER,
  has_plain_lyrics BOOLEAN, has_synced_lyrics BOOLEAN, instrumental BOOLEAN, source TEXT, created_at DATETIME,
  updated_at DATETIME, FOREIGN KEY (track_id) REFERENCES tracks (id)
);
CREATE TABLE missing_tracks (
  id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, name_lower TEXT, artist_name TEXT, artist_name_lower TEXT,
  album_name TEXT, album_name_lower TEXT, duration FLOAT, created_at DATETIME, updated_at DATETIME
);
CREATE TABLE flags (id INTEGER PRIMARY KEY AUTOINCREMENT, lyrics_id INTEGER, content TEXT, created_at DATETIME);
CREATE INDEX idx_tracks_name_lower ON tracks (name_lower);
CREATE INDEX idx_lyrics_track_id ON lyrics (track_id);
";

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[usize::try_from(self.below(items.len() as u64)).unwrap()]
    }
}

const WORDS: &[&str] = &[
    "love",
    "Night",
    "don't",
    "Café",
    "rain",
    "fire",
    "(Live)",
    "- Remastered",
    "über",
    "我的",
    "Ñandú",
    "it's",
    "a",
    "the",
    "STOP!",
    "río",
    "Ærø",
    "#1",
    "love",
    "you",
    "me",
];
const ARTISTS: &[&str] = &[
    "The Band",
    "Beyoncé",
    "AC/DC",
    "Sigur Rós",
    "Röyksopp",
    "Various",
    "a",
    "Мумий Тролль",
    "The Band!",
];

fn sentence(rng: &mut Rng, words: u64) -> String {
    (0..=words)
        .map(|_| rng.pick(WORDS))
        .collect::<Vec<_>>()
        .join(" ")
}

fn lyrics_text(rng: &mut Rng) -> (String, String) {
    let lines = 2 + rng.below(30);
    let mut plain = Vec::new();
    let mut synced = Vec::new();
    let mut t = rng.below(10_000);
    for i in 0..lines {
        let n = 1 + rng.below(6);
        let line = sentence(rng, n);
        if i % 7 == 6 {
            plain.push(String::new());
        }
        plain.push(line.clone());
        t += 500 + rng.below(5000);
        let (m, s, cs) = (t / 60_000, (t / 1000) % 60, (t % 1000) / 10);
        synced.push(format!("[{m:02}:{s:02}.{cs:02}] {line}"));
    }
    (plain.join("\n"), synced.join("\n"))
}

fn build_db(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    // Columns LRCLIB's server added later; older rows don't have them.
    conn.execute_batch(
        "INSERT INTO lyrics (plain_lyrics, track_id, instrumental) VALUES ('old row', 1, 0);",
    )
    .unwrap();
    conn.execute_batch(
        "ALTER TABLE lyrics ADD COLUMN lyricsfile TEXT;
         ALTER TABLE lyrics ADD COLUMN has_lyricsfile BOOLEAN NOT NULL DEFAULT FALSE;
         ALTER TABLE lyrics ADD COLUMN has_word_sync BOOLEAN NOT NULL DEFAULT FALSE;",
    )
    .unwrap();
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut previous_lyrics: Vec<i64> = vec![1];
    for i in 1..=3000_i64 {
        let artist = if i % 3 == 0 {
            "Prolific Artist".to_owned()
        } else {
            format!("{} {}", rng.pick(ARTISTS), rng.below(40))
        };
        let n = rng.below(4);
        let name = sentence(&mut rng, n);
        let n = rng.below(3);
        let album = sentence(&mut rng, n);
        let (name, album): (Option<String>, Option<String>) = match i % 97 {
            0 => (None, Some(album)),
            1 => (Some(name), None),
            2 => (Some(name), Some(String::new())),
            _ => (Some(name), Some(album)),
        };
        let mut name_lower = name.as_deref().map(normalize);
        if i % 101 == 0 {
            // Normalized by an older version of the server.
            name_lower = name.as_deref().map(|n| n.to_lowercase().replace(' ', "\0"));
        }
        let duration: Option<f64> = match i % 89 {
            0 => None,
            #[allow(clippy::cast_precision_loss)]
            _ => Some(30.0 + (rng.below(400_000) as f64) / 1000.0),
        };
        conn.execute(
            "INSERT INTO tracks (id, name, name_lower, artist_name, artist_name_lower, album_name, album_name_lower,
             duration) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                i * 3,
                name,
                name_lower,
                artist,
                normalize(&artist),
                album,
                album.as_deref().map(normalize),
                duration
            ],
        )
        .unwrap();
        let track_id = i * 3;

        // Lyrics of every kind.
        let (plain, synced) = lyrics_text(&mut rng);
        let kind = rng.below(12);
        let (plain, synced, instrumental): (Option<String>, Option<String>, bool) = match kind {
            0 => (None, None, true),
            1 => (Some(plain), None, false),
            2 => (None, Some(synced), false),
            3 => (
                lrclib_mirror::text::plain_from_lrc(&synced),
                Some(synced),
                false,
            ),
            4 => (
                Some(plain),
                Some(format!("[ar: x]\n{synced}\n[00:99.00] extra")),
                false,
            ),
            _ => (Some(plain), Some(synced), false),
        };
        let meta = lyricsfile::Meta {
            title: name.as_deref().unwrap_or_default(),
            artist: &artist,
            album: album.as_deref().unwrap_or_default(),
            duration: duration.unwrap_or_default(),
        };
        let built = lyricsfile::build(&meta, plain.as_deref(), synced.as_deref(), instrumental);
        let (lf, word_sync): (Option<String>, bool) = match rng.below(10) {
            0 => (None, false),
            1 => (
                Some(
                    "version: '1.0'\nlines:\n- text: Hi\n  start_ms: 1\n  words:\n  - text: Hi\n"
                        .to_owned(),
                ),
                true,
            ),
            2 => {
                // Built for other metadata (the track was renamed since).
                let other = lyricsfile::Meta {
                    title: "Old title",
                    ..meta
                };
                (
                    lyricsfile::build(&other, plain.as_deref(), synced.as_deref(), instrumental),
                    false,
                )
            }
            _ => (built, false),
        };
        let lyrics_id = match i % 50 {
            // Lyrics shared with an earlier track.
            0 => *previous_lyrics.last().unwrap(),
            // Lyrics that aren't in the dump.
            1 => 999_999,
            _ => {
                if i % 13 == 0 {
                    // An older version no track points to anymore.
                    conn.execute(
                        "INSERT INTO lyrics (plain_lyrics, track_id, instrumental) VALUES ('stale', ?, 0)",
                        [track_id],
                    )
                    .unwrap();
                }
                conn.execute(
                    "INSERT INTO lyrics (plain_lyrics, synced_lyrics, track_id, instrumental, lyricsfile,
                     has_lyricsfile, has_word_sync) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    params![plain, synced, track_id, instrumental, lf, lf.is_some(), word_sync],
                )
                .unwrap();
                conn.last_insert_rowid()
            }
        };
        previous_lyrics.push(lyrics_id);
        if i % 400 != 0 {
            conn.execute(
                "UPDATE tracks SET last_lyrics_id = ? WHERE id = ?",
                params![lyrics_id, track_id],
            )
            .unwrap();
        }
        if i % 10 == 0 {
            conn.execute(
                "INSERT INTO missing_tracks (name, name_lower, artist_name) VALUES (?, ?, ?)",
                params![name, name_lower, artist],
            )
            .unwrap();
        }
    }
    // A track LRCLIB's server can't read: a blob where text belongs.
    conn.execute_batch(
        "INSERT INTO tracks (id, name, name_lower, artist_name, artist_name_lower, duration)
         VALUES (99999, X'3432', '42', 'Broken', 'broken', 100.0);",
    )
    .unwrap();
    conn.execute_batch("VACUUM").unwrap();
    conn
}

/// What LRCLIB's server answers for a track (`None`: an error).
#[allow(clippy::type_complexity)]
fn oracle(
    conn: &Connection,
    id: i64,
) -> Option<(
    Option<String>,
    Option<String>,
    Option<String>,
    Option<f64>,
    bool,
    bool,
    Option<String>,
    Option<String>,
    Option<String>,
)> {
    conn.query_row(
        "SELECT tracks.name, tracks.artist_name, tracks.album_name, tracks.duration, lyrics.instrumental,
           lyrics.has_word_sync, lyrics.plain_lyrics, lyrics.synced_lyrics, lyrics.lyricsfile
         FROM tracks LEFT JOIN lyrics ON tracks.last_lyrics_id = lyrics.id WHERE tracks.id = ?",
        [id],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get::<_, Option<bool>>(4)?.unwrap_or(false),
                r.get::<_, Option<bool>>(5)?.unwrap_or(false),
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
            ))
        },
    )
    .ok()
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lrclib-mirror-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn imports_and_answers_like_lrclib() {
    let dir = tmp("store");
    let db = dir.join("dump.sqlite3");
    let conn = build_db(&db);
    let out = dir.join("store");
    let options = ImportOptions {
        threads: 3,
        level: 3,
        index_memory: 64 << 20,
        memory: 16 << 20,
    };
    let input = std::fs::File::open(&db).unwrap();
    let stats = import(input, &out, &options, &AtomicBool::new(false)).unwrap();
    assert_eq!(stats.tracks, 3001);
    assert!(stats.lyrics_unused > 0, "{stats:?}");
    assert!(stats.lyrics_missing > 0, "{stats:?}");
    assert!(
        stats.plain_derived > 0 && stats.synced_skeleton > 0,
        "{stats:?}"
    );
    assert!(
        stats.lyricsfile_built > 0 && stats.lyricsfile_head > 0 && stats.lyricsfile_text > 0,
        "{stats:?}"
    );
    assert!(stats.lower_exceptions > 0, "{stats:?}");
    assert_eq!(stats.broken_tracks, 1, "{stats:?}");

    let store = Store::open(&out).unwrap();
    let ids: Vec<i64> = conn
        .prepare("SELECT id FROM tracks ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for &id in &ids {
        let ordinal = store
            .ordinal(id)
            .unwrap()
            .unwrap_or_else(|| panic!("track {id}"));
        let expected = oracle(&conn, id);
        with_scratch(|s| {
            let track = store.track(ordinal, &mut s.tracks).unwrap();
            assert_eq!(i64::from(track.id), id);
            let Some(e) = &expected else {
                assert!(track.broken, "track {id}");
                return;
            };
            assert!(!track.broken);
            let lyrics = store.lyrics(ordinal, &track, &mut s.lyrics).unwrap();
            let got = (
                track.name.map(str::to_owned),
                track.artist.map(str::to_owned),
                track.album.map(str::to_owned),
                track.duration,
                lyrics.instrumental,
                lyrics.word_sync,
                lyrics.plain.map(std::borrow::Cow::into_owned),
                lyrics.synced.map(std::borrow::Cow::into_owned),
                lyrics.lyricsfile.map(std::borrow::Cow::into_owned),
            );
            assert_eq!(&got, e, "track {id}");
        });
    }
    for id in [0, 1, 2, 4, 9001, 100_000, -5, i64::MAX] {
        assert_eq!(store.ordinal(id).unwrap(), None, "id {id}");
    }

    // Lookups by metadata, as LRCLIB's server makes them: by every track's own normalized names, with and without
    // album and duration.
    let mut rows = conn
        .prepare("SELECT name_lower, artist_name_lower, album_name_lower, duration FROM tracks")
        .unwrap();
    let keys: Vec<(Option<String>, Option<String>, Option<String>, Option<f64>)> = rows
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut checked = 0;
    for (name, artist, album, duration) in keys {
        let (Some(name), Some(artist)) = (name, artist) else {
            continue;
        };
        for (album, duration) in [
            (None, None),
            (album.clone(), None),
            (None, duration.map(|d| d + 1.9)),
            (album.clone(), duration.map(|d| d - 2.1)),
        ] {
            // As LRCLIB's server builds it.
            let mut sql = String::from(
                "SELECT id FROM tracks WHERE name_lower = ? AND artist_name_lower = ?",
            );
            let mut values: Vec<rusqlite::types::Value> =
                vec![name.clone().into(), artist.clone().into()];
            if let Some(d) = duration {
                sql.push_str(" AND duration >= ? AND duration <= ?");
                values.push((d - 2.0).into());
                values.push((d + 2.0).into());
            }
            if let Some(a) = &album {
                sql.push_str(" AND album_name_lower = ?");
                values.push(a.clone().into());
            }
            sql.push_str(" ORDER BY id LIMIT 1");
            let expected: Option<i64> = conn
                .query_row(&sql, rusqlite::params_from_iter(values), |r| r.get(0))
                .optional()
                .unwrap();
            let got = with_scratch(|s| {
                let ordinal = store
                    .find(&name, &artist, album.as_deref(), duration, &mut s.tracks)
                    .unwrap()?;
                Some(i64::from(store.track(ordinal, &mut s.tracks).unwrap().id))
            });
            assert_eq!(got, expected, "{name:?} {artist:?} {album:?} {duration:?}");
            checked += 1;
        }
    }
    assert!(checked > 10_000);

    // The search index has every track.
    let found = store
        .search
        .search(None, Some("love"), Some("prolific artist"), None)
        .unwrap();
    assert_ne!(found, Vec::<i64>::new());
    std::fs::remove_dir_all(&dir).unwrap();
}
