//! Imports a small database with LRCLIB's schema through the streaming importer, and checks every track against
//! SQLite answering the queries LRCLIB's server makes.

// A test: panicking on the unexpected is the point.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::type_complexity
)]

mod common;

use common::{build_db, tmp};
use std::sync::atomic::AtomicBool;

use lrclib_mirror::api;
use lrclib_mirror::store::{ImportOptions, Store, import, with_scratch};
use lrclib_mirror::text::normalize;
use rusqlite::{Connection, OptionalExtension as _};

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

/// Request parameters from JSON.
fn params<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).unwrap()
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
        memory: 3 << 20,
        checkpoint_every: 128,
    };
    let input = std::fs::File::open(&db).unwrap();
    let stats = import(Some(input), &out, &options, &AtomicBool::new(false)).unwrap();
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
    // The prolific artist's bucket is larger than its share of the memory.
    assert!(stats.split_buckets > 0, "{stats:?}");

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
            // Which lyrics there are, without computing them back.
            let flags = store.lyrics_flags(ordinal, &mut s.lyrics).unwrap();
            assert_eq!(
                (
                    flags.plain,
                    flags.synced,
                    flags.lyricsfile,
                    flags.instrumental,
                    flags.word_sync
                ),
                (e.6.is_some(), e.7.is_some(), e.8.is_some(), e.4, e.5),
                "track {id}"
            );
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

    // An artist's tracks: those with its normalized name.
    let artists: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT artist_name_lower FROM tracks WHERE artist_name_lower IS NOT NULL",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(artists.len() > 10);
    for artist in artists {
        let mut expected: Vec<i64> = conn
            .prepare("SELECT id FROM tracks WHERE artist_name_lower = ?")
            .unwrap()
            .query_map([&artist], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        expected.sort_unstable();
        let mut got = Vec::new();
        with_scratch(|s| {
            store
                .for_artist(&artist, &mut s.tracks, |_, track| {
                    got.push(i64::from(track.id));
                })
                .unwrap();
        });
        got.sort_unstable();
        assert_eq!(got, expected, "{artist:?}");
    }

    // The web search's lookups.
    let json = |body: Result<api::JsonBody, api::ApiError>| -> serde_json::Value {
        serde_json::from_slice(body.unwrap().as_bytes()).unwrap()
    };
    let ids = |v: &serde_json::Value| -> Vec<i64> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_i64().unwrap())
            .collect()
    };
    let query = serde_json::json!({"track_name": "love", "artist_name": "prolific artist"});
    let v1 = json(api::search(&store, &params(query.clone())));
    let v2 = json(api::search_v2(&store, &params(query.clone())));
    assert_eq!(ids(&v2), ids(&v1));
    for (full, summary) in v1.as_array().unwrap().iter().zip(v2.as_array().unwrap()) {
        let keys: Vec<&str> = summary
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        // (serde_json's map sorts the keys.)
        assert_eq!(
            keys,
            [
                "albumName",
                "artistName",
                "duration",
                "hasLyricsfile",
                "hasPlainLyrics",
                "hasSyncedLyrics",
                "hasWordSync",
                "id",
                "instrumental",
                "trackName",
            ]
        );
        for (key, of) in [
            ("hasPlainLyrics", "plainLyrics"),
            ("hasSyncedLyrics", "syncedLyrics"),
            ("hasLyricsfile", "lyricsfile"),
        ] {
            assert_eq!(summary[key].as_bool().unwrap(), !full[of].is_null());
        }
    }
    // Pages follow the ranking past the 20 of LRCLIB's search.
    let mut paged = Vec::new();
    for offset in (0..).step_by(7) {
        let mut q = query.clone();
        q["offset"] = offset.into();
        q["limit"] = 7.into();
        let page = ids(&json(api::search_page(&store, &params(q))));
        let last = page.len() < 7;
        paged.extend(page);
        if last {
            break;
        }
    }
    assert!(paged.len() > 20, "{}", paged.len());
    assert_eq!(paged[..20], ids(&v1)[..]);
    assert!(
        api::search_page(
            &store,
            &params(serde_json::json!({"q": "love", "limit": 0}))
        )
        .is_err()
    );
    // An artist's tracks, by name, in pages.
    let artist = json(api::artist(
        &store,
        &params(serde_json::json!({"artist_name": "PROLIFIC  Artist", "limit": 100})),
    ));
    let total = usize::try_from(artist["total"].as_u64().unwrap()).unwrap();
    assert!(total > 100, "{total}");
    let names: Vec<String> = artist["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| normalize(t["trackName"].as_str().unwrap_or_default()))
        .collect();
    assert_eq!(names.len(), 100);
    assert!(names.is_sorted());
    let rest = json(api::artist(
        &store,
        &params(serde_json::json!({"artist_name": "prolific artist", "offset": 100, "limit": 100})),
    ));
    assert_eq!(rest["total"], artist["total"]);
    assert!(
        ids(&rest["tracks"])
            .iter()
            .all(|id| !ids(&artist["tracks"]).contains(id))
    );

    // The search index has every track.
    let found = store
        .search
        .search(None, Some("love"), Some("prolific artist"), None)
        .unwrap();
    assert_ne!(found, Vec::<i64>::new());
    std::fs::remove_dir_all(&dir).unwrap();
}
