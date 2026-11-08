//! Compares lrclib-mirror with LRCLIB's server, both answering from the same dump: every request is sent to both, and
//! the answers must be the same; then both are timed on requests neither has seen.
//!
//! The requests are made from tracks of the store (random, from a seed): lookups by id, by metadata (with and without
//! album and duration, with other spellings), searches (by words, by fields), and requests that must fail.
//!
//! Search results that differ only in the order of equally scored tracks are the same answer (LRCLIB's own order of
//! ties depends on how its index happens to be split into segments): the scores of both lists, as the mirror's index
//! computes them, must be equal position by position.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use lrclib_mirror::store::{Store, with_scratch};
use lrclib_mirror::text::normalize_param;

const HELP: &str = "\
lrclib-compare: compares lrclib-mirror with LRCLIB's server on the same dump

Usage: lrclib-compare --old URL --new URL --store DIR [--samples N] [--concurrency N] [--seed N]

  --old URL          LRCLIB's server (e.g. http://127.0.0.1:3397)
  --new URL          lrclib-mirror (e.g. http://127.0.0.1:3300)
  --store DIR        lrclib-mirror's store, to pick tracks and score searches
  --samples N        tracks to make requests from [1000]
  --concurrency N    requests in flight when timing under load [16]
  --seed N           random seed [1]";

#[derive(Clone, Debug)]
struct Request {
    kind: &'static str,
    path: String,
    /// For searches: (q, track_name, artist_name, album_name), to score results.
    search: Option<[Option<String>; 4]>,
}

/// xorshift64*: deterministic, good enough to pick samples.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn query(kind: &'static str, base: &str, params: &[(&str, &str)]) -> Request {
    let qs: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", enc(v)))
        .collect();
    let search = (base == "/api/search").then(|| {
        let get = |k: &str| {
            params
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        };
        [
            get("q"),
            get("track_name"),
            get("artist_name"),
            get("album_name"),
        ]
    });
    Request {
        kind,
        path: format!("{base}?{}", qs.join("&")),
        search,
    }
}

/// Requests made from `samples` random tracks.
fn requests(store: &Store, samples: usize, rng: &mut Rng) -> Vec<Request> {
    let mut out = Vec::new();
    let tracks = u64::from(store.meta.tracks);
    for i in 0..samples {
        #[allow(clippy::cast_possible_truncation)]
        let ordinal = rng.below(tracks) as u32;
        let t = with_scratch(|s| {
            store.track(ordinal, &mut s.tracks).map(|t| {
                (
                    t.id,
                    t.name.map(str::to_owned),
                    t.artist.map(str::to_owned),
                    t.album.map(str::to_owned),
                    t.duration,
                )
            })
        });
        let Ok((id, name, artist, album, duration)) = t else {
            continue;
        };
        out.push(Request {
            kind: "get by id",
            path: format!("/api/get/{id}"),
            search: None,
        });
        let (Some(name), Some(artist)) = (name, artist) else {
            continue;
        };
        out.push(query(
            "get",
            "/api/get",
            &[("track_name", &name), ("artist_name", &artist)],
        ));
        let mut full = vec![
            ("track_name", name.as_str()),
            ("artist_name", artist.as_str()),
        ];
        if let Some(album) = &album {
            full.push(("album_name", album));
        }
        let d = duration.map(|d| format!("{}", d + 1.5));
        if let Some(d) = &d {
            full.push(("duration", d));
        }
        out.push(query("get +album +duration", "/api/get", &full));
        out.push(query(
            "get (other spelling)",
            "/api/get",
            &[
                ("track_name", &name.to_uppercase()),
                ("artist_name", &format!(" {artist}!")),
            ],
        ));
        out.push(query(
            "search q",
            "/api/search",
            &[("q", &format!("{name} {artist}"))],
        ));
        out.push(query(
            "search fields",
            "/api/search",
            &[("track_name", &name), ("artist_name", &artist)],
        ));
        if i % 5 == 0 {
            // A single, often common, word: many matches to rank.
            let word = name.split_whitespace().next().unwrap_or("love").to_owned();
            out.push(query("search 1 word", "/api/search", &[("q", &word)]));
        }
    }
    out
}

/// Requests that must fail, or find nothing, the same way.
fn edge_cases() -> Vec<Request> {
    let mut out: Vec<Request> = [
        "/api/get/0",
        "/api/get/-1",
        "/api/get/999999999999",
        "/api/get/abc",
        "/api/get/1.5",
    ]
    .into_iter()
    .map(|p| Request {
        kind: "edge",
        path: p.to_owned(),
        search: None,
    })
    .collect();
    for qs in [
        "",
        "track_name=x",
        "artist_name=x",
        "track_name=&artist_name=x",
        "track_name=x&artist_name=",
        "track_name=&artist_name=",
        "track_name=!!!&artist_name=x",
        "track_name=x&artist_name=y&duration=0",
        "track_name=x&artist_name=y&duration=3601",
        "track_name=x&artist_name=y&duration=abc",
        "track_name=x&artist_name=y&duration=NaN",
        "track_name=nothing%20like%20this%20exists%20zzzq&artist_name=nobody",
    ] {
        out.push(Request {
            kind: "edge",
            path: format!("/api/get?{qs}"),
            search: None,
        });
    }
    let searches: [&[(&str, &str)]; 8] = [
        &[],
        &[("q", "")],
        &[("q", "!!!")],
        &[("artist_name", "queen")],
        &[("q", "the")],
        &[("q", "love")],
        &[("q", "love you")],
        &[("track_name", "a"), ("artist_name", "!")],
    ];
    for params in searches {
        out.push(query("edge", "/api/search", params));
    }
    out
}

struct Answer {
    status: u16,
    content_type: String,
    body: Vec<u8>,
    took: Duration,
}

async fn fetch(client: &reqwest::Client, base: &str, path: &str) -> Result<Answer, String> {
    let started = Instant::now();
    let response = client
        .get(format!("{base}{path}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = response.bytes().await.map_err(|e| e.to_string())?.to_vec();
    Ok(Answer {
        status,
        content_type,
        body,
        took: started.elapsed(),
    })
}

/// Whether two answers are the same; for searches, up to the order of ties.
fn same(store: &Store, request: &Request, old: &Answer, new: &Answer) -> Result<(), String> {
    if old.status != new.status {
        return Err(format!("status {} vs {}", old.status, new.status));
    }
    if old.content_type != new.content_type {
        return Err(format!(
            "content type {:?} vs {:?}",
            old.content_type, new.content_type
        ));
    }
    if old.body == new.body {
        return Ok(());
    }
    let (Ok(o), Ok(n)) = (
        serde_json::from_slice::<serde_json::Value>(&old.body),
        serde_json::from_slice::<serde_json::Value>(&new.body),
    ) else {
        return Err(format!(
            "bodies differ: {:?} vs {:?}",
            String::from_utf8_lossy(&old.body),
            String::from_utf8_lossy(&new.body)
        ));
    };
    // Validation messages list invalid fields in no particular order.
    if let (Some(om), Some(nm)) = (
        o.get("message").and_then(|m| m.as_str()),
        n.get("message").and_then(|m| m.as_str()),
    ) {
        let mut a: Vec<_> = om.lines().collect();
        let mut b: Vec<_> = nm.lines().collect();
        a.sort_unstable();
        b.sort_unstable();
        if a == b && o.get("name") == n.get("name") {
            return Ok(());
        }
    }
    let (Some(ol), Some(nl)) = (o.as_array(), n.as_array()) else {
        return Err(format!("bodies differ: {} vs {}", short(&o), short(&n)));
    };
    if ol.len() != nl.len() {
        return Err(format!("{} vs {} results", ol.len(), nl.len()));
    }
    // Same results, maybe in another order: each must be identical, and the scores must agree by position.
    let by_id = |l: &[serde_json::Value]| -> BTreeMap<i64, serde_json::Value> {
        l.iter()
            .filter_map(|v| Some((v.get("id")?.as_i64()?, v.clone())))
            .collect()
    };
    let (om, nm) = (by_id(ol), by_id(nl));
    for (id, v) in &nm {
        if let Some(ov) = om.get(id)
            && ov != v
        {
            return Err(format!("track {id} differs: {} vs {}", short(ov), short(v)));
        }
    }
    let Some([q, t, a, al]) = &request.search else {
        return Err("bodies differ".to_owned());
    };
    let norm = |s: &Option<String>| normalize_param(s.as_deref());
    let (q, t, a, al) = (norm(q), norm(t), norm(a), norm(al));
    let score = |v: &serde_json::Value| -> Option<f32> {
        let id = v.get("id")?.as_i64()?;
        store
            .search
            .score(id, q.as_deref(), t.as_deref(), a.as_deref(), al.as_deref())
            .ok()
            .flatten()
    };
    for (i, (ov, nv)) in ol.iter().zip(nl).enumerate() {
        let (os, ns) = (score(ov), score(nv));
        let close = match (os, ns) {
            (Some(x), Some(y)) => (x - y).abs() <= 1e-4 * x.abs().max(1.0),
            _ => false,
        };
        if !close {
            return Err(format!("result {i}: scores {os:?} vs {ns:?}"));
        }
    }
    Ok(())
}

fn short(v: &serde_json::Value) -> String {
    let s = v.to_string();
    if s.len() > 300 {
        format!("{}…", s.chars().take(300).collect::<String>())
    } else {
        s
    }
}

#[derive(Default)]
struct Timings(BTreeMap<&'static str, hdrhistogram::Histogram<u64>>);

impl Timings {
    fn record(&mut self, kind: &'static str, took: Duration) {
        let h = self
            .0
            .entry(kind)
            .or_insert_with(|| hdrhistogram::Histogram::new(3).unwrap_or_else(|_| unreachable!()));
        let _ = h.record(u64::try_from(took.as_micros()).unwrap_or(u64::MAX).max(1));
    }
}

/// Times `requests` against `base`, `concurrency` at a time.
async fn time(
    client: &reqwest::Client,
    base: &str,
    requests: &[Request],
    concurrency: usize,
) -> (Timings, Duration, usize) {
    let started = Instant::now();
    let mut timings = Timings::default();
    let mut failed = 0;
    let mut answers = futures::stream::iter(
        requests
            .iter()
            .map(|r| async move { (r.kind, fetch(client, base, &r.path).await) }),
    )
    .buffer_unordered(concurrency);
    while let Some((kind, answer)) = answers.next().await {
        match answer {
            Ok(a) if a.status < 500 => timings.record(kind, a.took),
            _ => failed += 1,
        }
    }
    (timings, started.elapsed(), failed)
}

fn ms(us: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let ms = us as f64 / 1000.0;
    format!("{ms:.2}")
}

fn report(out: &mut impl std::io::Write, title: &str, old: &Timings, new: &Timings) {
    let _ = writeln!(out, "\n{title} (ms)\n");
    let _ = writeln!(
        out,
        "| request | n | old p50 | new p50 | old p90 | new p90 | old p99 | new p99 | old max | new max |"
    );
    let _ = writeln!(
        out,
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    for (kind, n) in &new.0 {
        let Some(o) = old.0.get(kind) else { continue };
        let _ = writeln!(
            out,
            "| {kind} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            n.len(),
            ms(o.value_at_quantile(0.5)),
            ms(n.value_at_quantile(0.5)),
            ms(o.value_at_quantile(0.9)),
            ms(n.value_at_quantile(0.9)),
            ms(o.value_at_quantile(0.99)),
            ms(n.value_at_quantile(0.99)),
            ms(o.max()),
            ms(n.max()),
        );
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let mut args = pico_args::Arguments::from_env();
    if args.contains(["-h", "--help"]) {
        let _ = writeln!(std::io::stdout(), "{HELP}");
        return std::process::ExitCode::SUCCESS;
    }
    let parsed = (|| -> Result<_, pico_args::Error> {
        Ok((
            args.value_from_str::<_, String>("--old")?,
            args.value_from_str::<_, String>("--new")?,
            args.value_from_str::<_, PathBuf>("--store")?,
            args.opt_value_from_str::<_, usize>("--samples")?
                .unwrap_or(1000),
            args.opt_value_from_str::<_, usize>("--concurrency")?
                .unwrap_or(16),
            args.opt_value_from_str::<_, u64>("--seed")?.unwrap_or(1),
        ))
    })();
    let (old, new, store, samples, concurrency, seed) = match parsed {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "lrclib-compare: {e}\n\n{HELP}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let store = match Store::open(&store) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "lrclib-compare: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(concurrency * 2)
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_default();
    let mut out = std::io::stdout();
    let mut rng = Rng(seed.max(1).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);

    // Timing first, on requests neither has answered (LRCLIB's server caches answers): one at a time, then under load.
    // Lookups with another spelling are left out: LRCLIB's server answers them from its cache (they normalize to a
    // lookup just made).
    let timing = |r: &&Request| r.kind != "get (other spelling)";
    let all_timed = requests(&store, samples, &mut rng);
    let all_loaded = requests(&store, samples, &mut rng);
    let timed: Vec<Request> = all_timed.iter().filter(timing).cloned().collect();
    let loaded: Vec<Request> = all_loaded.iter().filter(timing).cloned().collect();
    let _ = writeln!(
        out,
        "# lrclib-compare\n\n{} requests timed one at a time, {} under load ({concurrency} in flight)",
        timed.len(),
        loaded.len()
    );
    let (old_t, old_total, old_failed) = time(&client, &old, &timed, 1).await;
    let (new_t, new_total, new_failed) = time(&client, &new, &timed, 1).await;
    report(&mut out, "One at a time", &old_t, &new_t);
    let _ = writeln!(
        out,
        "\nTotal: old {old_total:.1?} ({old_failed} failed), new {new_total:.1?} ({new_failed} failed)"
    );
    let (old_l, old_total, old_failed) = time(&client, &old, &loaded, concurrency).await;
    let (new_l, new_total, new_failed) = time(&client, &new, &loaded, concurrency).await;
    report(
        &mut out,
        &format!("Under load, {concurrency} in flight"),
        &old_l,
        &new_l,
    );
    #[allow(clippy::cast_precision_loss)]
    let rate = |n: usize, d: Duration| n as f64 / d.as_secs_f64();
    let _ = writeln!(
        out,
        "\nThroughput: old {:.0}/s ({old_failed} failed), new {:.0}/s ({new_failed} failed)",
        rate(loaded.len(), old_total),
        rate(loaded.len(), new_total)
    );

    // Then the answers: every request of both sets, and the edge cases.
    let mut all = all_timed;
    all.extend(all_loaded);
    all.extend(edge_cases());
    let mut mismatches: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    let mut shown = 0;
    let checks = futures::stream::iter(all.iter().map(|r| {
        let client = &client;
        let (old, new) = (&old, &new);
        async move {
            (
                r,
                fetch(client, old, &r.path).await,
                fetch(client, new, &r.path).await,
            )
        }
    }))
    .buffer_unordered(concurrency);
    let results: Vec<_> = checks.collect().await;
    let _ = writeln!(out, "\n## Answers\n");
    for (request, o, n) in results {
        let entry = mismatches.entry(request.kind).or_default();
        entry.0 += 1;
        let verdict = match (o, n) {
            (Ok(o), Ok(n)) => same(&store, request, &o, &n),
            (Err(e), _) | (_, Err(e)) => Err(e),
        };
        if let Err(e) = verdict {
            entry.1 += 1;
            if shown < 20 {
                shown += 1;
                let _ = writeln!(out, "- MISMATCH `{}`: {e}", request.path);
            }
        }
    }
    let _ = writeln!(
        out,
        "\n| request | compared | different |\n| --- | ---: | ---: |"
    );
    let mut different = 0;
    for (kind, (n, bad)) in &mismatches {
        different += bad;
        let _ = writeln!(out, "| {kind} | {n} | {bad} |");
    }
    let _ = writeln!(out, "\n{} of {} answers differ", different, all.len());
    if different == 0 {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
