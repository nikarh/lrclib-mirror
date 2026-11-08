//! A small database with LRCLIB's schema and rows of every kind, for the tests.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    dead_code
)]

use std::path::{Path, PathBuf};

use lrclib_mirror::lyricsfile;
use lrclib_mirror::text::normalize;
use rusqlite::{Connection, params};

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

pub fn build_db(path: &Path) -> Connection {
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

pub fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lrclib-mirror-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
