//! Imports killed at any point and run again finish as an import that was never interrupted: the same store, byte for
//! byte, answering the same.

// A test: panicking on the unexpected is the point.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{build_db, tmp};
use lrclib_mirror::store::{Store, with_scratch};

fn import(db: &Path, out: &Path) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_lrclib-mirror"))
        .args([
            "import",
            &db.display().to_string(),
            &out.display().to_string(),
        ])
        .args(["--import-threads", "2"])
        // Checkpoints often, so that interruptions land between them.
        .env("LRCLIB_MIRROR_CHECKPOINT_EVERY", "8")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// The files that must be the same byte for byte (the search index, Tantivy's, is compared by what it answers).
const SAME: &[&str] = &[
    "tracks.zst",
    "tracks.idx",
    "tracks.dict",
    "lyrics.zst",
    "lyrics.idx",
    "lyrics.dict",
    "ids.bin",
];

fn assert_same(reference: &Path, other: &Path) {
    for name in SAME {
        assert!(
            std::fs::read(reference.join(name)).unwrap()
                == std::fs::read(other.join(name)).unwrap(),
            "{name} differs in {}",
            other.display()
        );
    }
    let (a, b) = (Store::open(reference).unwrap(), Store::open(other).unwrap());
    for q in ["love", "a", "the band", "prolific artist", "café"] {
        assert_eq!(
            a.search.search(Some(q), None, None, None).unwrap().len(),
            b.search.search(Some(q), None, None, None).unwrap().len(),
            "{q}"
        );
    }
    for ordinal in (0..a.meta.tracks).step_by(97) {
        with_scratch(|s| {
            let (ta, tb) = (
                a.track(ordinal, &mut s.tracks).unwrap().id,
                b.track(ordinal, &mut s.tracks).unwrap().id,
            );
            assert_eq!(ta, tb);
        });
    }
}

#[test]
fn interrupted_imports_finish_the_same() {
    let dir = tmp("interrupt");
    let db = dir.join("dump.sqlite3");
    drop(build_db(&db));

    let reference = dir.join("reference");
    let started = Instant::now();
    assert!(import(&db, &reference).wait().unwrap().success());
    let whole = started.elapsed();

    // Killed once at each of several points of an import, then again early in each restart, then run to the end.
    let mut resumed_pass2 = false;
    for step in 1..=6 {
        let out = dir.join(format!("out-{step}"));
        let mut kill_at = whole * step / 7;
        let mut runs = 0;
        loop {
            runs += 1;
            let mut child = import(&db, &out);
            if runs <= 3 {
                std::thread::sleep(kill_at);
                if child.try_wait().unwrap().is_none() {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    resumed_pass2 |= std::fs::read_to_string(out.join("import.json"))
                        .is_ok_and(|p| p.contains("\"buckets\""));
                    kill_at = (kill_at / 2).max(Duration::from_millis(20));
                    continue;
                }
            }
            assert!(child.wait().unwrap().success(), "step {step}, run {runs}");
            break;
        }
        assert_same(&reference, &out);
        // Nothing of the import is left.
        for leftover in std::fs::read_dir(&out).unwrap().flatten() {
            let name = leftover.file_name().to_string_lossy().into_owned();
            assert!(
                !(name == "tmp"
                    || name == "import.json"
                    || Path::new(&name).extension().is_some_and(|e| e == "partial")
                    || Path::new(&name).extension().is_some_and(|e| e == "tmp")),
                "{name} left in step {step}"
            );
        }
    }
    assert!(resumed_pass2, "no interruption landed after a checkpoint");
    std::fs::remove_dir_all(&dir).unwrap();
}
