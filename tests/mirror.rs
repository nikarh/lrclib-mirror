//! What the mirror makes of its data directory at start: what was left by a crash, finished imports, damaged stores,
//! and another process.

// A test: panicking on the unexpected is the point.
#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use common::{build_db, tmp};
use lrclib_mirror::config::Config;
use lrclib_mirror::mirror::{Mirror, MirrorError};
use lrclib_mirror::store::{ImportOptions, import};

const OLD: &str = "lrclib-db-dump-20260101T000000Z";
const NEW: &str = "lrclib-db-dump-20260201T000000Z";
const NEWER: &str = "lrclib-db-dump-20260301T000000Z";

fn config(data: &Path) -> Config {
    Config {
        data_dir: data.to_owned(),
        listen: "127.0.0.1:0".parse().unwrap(),
        // Nothing is downloaded: the listing is never asked for.
        update_interval: Some(Duration::from_hours(1000)),
        min_free_bytes: 0,
        import_threads: 2,
        import_memory_bytes: None,
        listing_url: "http://127.0.0.1:9/".to_owned(),
        dump_url: "http://127.0.0.1:9".to_owned(),
    }
}

fn build(db: &Path, out: &Path) {
    let options = ImportOptions::new(2, 1 << 30);
    import(
        Some(std::fs::File::open(db).unwrap()),
        out,
        &options,
        &AtomicBool::new(false),
    )
    .unwrap();
}

fn served(mirror: &Mirror) -> Option<String> {
    mirror
        .current()
        .map(|s| s.dir().file_name().unwrap().to_string_lossy().into_owned())
}

#[tokio::test]
async fn start_tidies_up_after_a_crash() {
    let dir = tmp("mirror");
    let db = dir.join("dump.sqlite3");
    drop(build_db(&db));
    let data = dir.join("data");
    let stores = data.join("stores");
    std::fs::create_dir_all(&stores).unwrap();

    // An old store, a newer import finished but not moved, what a removal left.
    build(&db, &stores.join(OLD));
    build(&db, &stores.join(format!("{NEW}.partial")));
    std::fs::create_dir_all(stores.join(format!("{OLD}.removing/x"))).unwrap();

    let mirror = Mirror::new(config(&data)).unwrap();
    mirror.start().await.unwrap();
    assert_eq!(served(&mirror).as_deref(), Some(NEW));
    let mut left: Vec<_> = std::fs::read_dir(&stores)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, [NEW]);

    // One process at a time.
    let other = Mirror::new(config(&data)).unwrap();
    assert!(matches!(other.start().await, Err(MirrorError::Locked(_))));
    mirror.shutdown().await;
    drop(mirror);

    // A newer store, damaged (a file shorter than written): removed, the older one served.
    std::fs::rename(stores.join(NEW), stores.join(OLD)).unwrap();
    build(&db, &stores.join(NEWER));
    let lyrics = stores.join(NEWER).join("lyrics.zst");
    let len = std::fs::metadata(&lyrics).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&lyrics)
        .unwrap()
        .set_len(len - 1)
        .unwrap();
    let mirror = Mirror::new(config(&data)).unwrap();
    mirror.start().await.unwrap();
    assert_eq!(served(&mirror).as_deref(), Some(OLD));
    assert!(!stores.join(NEWER).exists());

    // An unfinished store under a store's name (no meta.json) is damaged too.
    mirror.shutdown().await;
    drop(mirror);
    std::fs::remove_file(stores.join(OLD).join("meta.json")).unwrap();
    let mirror = Mirror::new(config(&data)).unwrap();
    mirror.start().await.unwrap();
    assert_eq!(served(&mirror), None);
    assert!(!stores.join(OLD).exists());
    std::fs::remove_dir_all(&dir).unwrap();
}
