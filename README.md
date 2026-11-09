# lrclib-mirror

A self-hosted, read-only [LRCLIB](https://lrclib.net): LRCLIB's lookup API and its web search, answered from a local
copy of LRCLIB's [public database dump](https://lrclib.net/db-dumps), which it keeps up to date by itself. Useful when
a music library needs lyrics for many tracks: no rate limits, no network round trips.

The dump is a ~50 GB download that unpacks to a ~270 GB SQLite database (2026). lrclib-mirror never unpacks it:
it reads the rows out of the download as it streams in and builds a store of **9.3 GB** (search index included),
which answers the API as LRCLIB's server does, byte for byte, about twice as fast: a lookup in well under a
millisecond.

- `GET /api/get`, `GET /api/get/{id}`, `GET /api/search` answer exactly as on lrclib.net (same bodies, statuses and
  errors; search ranks the same). Publishing and flagging lyrics are left out: the next dump replaces the copy.
- The web search is [tranxuanthang/lrclib-homepage](https://github.com/tranxuanthang/lrclib-homepage) (the lrclib.net
  page, reduced to the search and the lyrics view, asking the local API), a git submodule changed only by the patch
  in `patches/`.
- Dumps are checked for daily; a newer one is imported in the background and switched to without a restart.

## How 271 GB become 9.3

LRCLIB's database keeps every version of every track's lyrics, each three times: plain, synced (LRC) and as a
Lyricsfile (YAML), plus indexes. The API only ever answers with a track's current lyrics, and most of what is stored
can be computed from the rest:

| What | Dump | Store |
| --- | --- | --- |
| Lyrics versions | 35.1 M | 34.6 M (only the current ones) |
| Lyricsfiles | 3.7 KB each, 60% of the bytes | ~97% are what LRCLIB's server builds from the track and its lyrics, and aren't stored; ~4% differ only in their metadata, which alone is kept |
| Synced lyrics | stored | ~62% repeat the plain lyrics line by line: only the timestamps are kept |
| Plain lyrics | stored | ~3% are the synced ones without timestamps, and aren't stored |
| Normalized names (for lookups) | stored, and indexed | computed (kept for the 0.03% LRCLIB normalized differently in the past) |

What is left is sorted by artist and title, so that an artist's songs, which often repeat across albums and
editions, sit next to each other, and compressed with zstd in blocks of 64 KB (with a dictionary trained on the
dump). Lookups need no index beyond the first key of each block; the search index is the one LRCLIB's server uses
(Tantivy, same schema and tokenizer, so the same BM25 scores), in a single segment.

Everything left out is checked when importing: a value is left out only when computing it back gives the stored one
byte for byte. The store is versioned, and a version of lrclib-mirror that would compute anything differently
imports the dump again rather than serve it (the API answers `503` until then).

| | Dump (SQLite + LRCLIB's search index) | Store |
| --- | --- | --- |
| Download | 49 GB (gzip) | the same |
| On disk | 268.6 GB + 2.5 GB | 9.25 GB: lyrics 6.80, tracks 0.90, search index 1.37, id map 0.16 |
| Disk while updating | old + new database, the new one unpacked: ~540 GB | old store + the new one and its temporary files: ~32 GB |

### Speed

Measured with `tools/compare` against LRCLIB's server on the same dump and the same machine (Intel Core Ultra 9
185H, NVMe SSD; LRCLIB's server on 8 cores, lrclib-mirror on 6), 2000 tracks' worth of requests neither had answered
before (LRCLIB's server caches answers), in milliseconds:

| One at a time | LRCLIB p50 | p90 | p99 | lrclib-mirror p50 | p90 | p99 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `/api/get` (name, artist) | 0.81 | 1.76 | 78.21 | **0.22** | **0.43** | **0.98** |
| `/api/get` (+ album, duration) | 0.69 | 0.93 | 1.36 | **0.20** | **0.33** | **0.96** |
| `/api/get/{id}` | 0.62 | 0.87 | 1.16 | **0.35** | **0.48** | **0.80** |
| `/api/search?q=` (name and artist) | 6.93 | 17.07 | 25.34 | **2.38** | **6.24** | **12.82** |
| `/api/search` (by fields) | 1.66 | 10.40 | 14.75 | **0.96** | **2.75** | **4.50** |
| `/api/search?q=` (one word) | 10.71 | 16.46 | 26.94 | **6.31** | **8.79** | **12.33** |

Under load (16 requests in flight) lrclib-mirror answers 5100 requests a second, LRCLIB's server 2450. A search
answers with up to 20 tracks with their lyrics; most of its time is ranking the words, common words take longest
(`q=the`: ~11 ms).

### Importing

The importer reads the dump once, as it downloads: it picks the rows out of the SQLite pages as they stream by
(the dump is never written to disk), spreads them over temporary bucket files by artist, then sorts, compresses and
indexes bucket by bucket. Measured on the 2026-10-30 dump (34.6 M tracks):

| Where | Time | Memory (peak) | Temporary disk (peak) |
| --- | ---: | ---: | ---: |
| The image, in a container limited to 4 CPUs and 2 GB, from the 49 GB `.gz` | 30 min | 1.4 GB | 22 GB |
| A native build on 4 cores, from the unpacked dump | 17 min | 3.6 GB | 22 GB |

Reading the dump (unpacking 270 GB of gzip, picking out the rows) takes two thirds of it; the unpacking is a single
stream, so more cores help the rest more than it.

It uses every CPU it is given (`--import-threads`, by default the CPUs of the machine or the container's quota) and
about 60% of the memory it may use (`--import-memory`, by default from the container's memory limit or the
machine's memory, between 768 MB and 4 GB). Less of either makes it slower, not fail: an artist with very many
tracks is sorted in parts.

An import survives being interrupted (the process killed, the machine losing power): run again, it continues from
its last checkpoint once the dump has been read, and starts over when it was still reading it (the dump is streamed,
never stored). A download that breaks is resumed where it stopped. A store is served only whole: it lists its files
and their sizes, checked when it is opened, and is removed only when shown to be damaged.

## Images and downloads

There are no versions, only builds of the newest code:

| | Image | Release (binary, signed checksums) |
| --- | --- | --- |
| `main` | `ghcr.io/nikarh/lrclib-mirror:main` or `:v2` (and `:v2-sha-<commit>`) | [Latest (`v2`)](https://github.com/nikarh/lrclib-mirror/releases/tag/v2) |
| pending update | `ghcr.io/nikarh/lrclib-mirror:dev` or `:v2-dev` | [Dev (`v2-dev`)](https://github.com/nikarh/lrclib-mirror/releases/tag/v2-dev) |

The previous version, which answered with LRCLIB's server from an SQLite copy of the dump (~270 GB), is no longer
updated: the `ghcr.io/nikarh/lrclib-mirror:v1` image (and `:v1-dev`, `:sha-<commit>`), the
[`v1`](https://github.com/nikarh/lrclib-mirror/releases/tag/v1) release, and its code on the
[`legacy-sqlite`](https://github.com/nikarh/lrclib-mirror/tree/legacy-sqlite) branch. Its data directory isn't
used by this version (`dumps/*.sqlite3` and `search-index/` can be removed once a store is imported).

The image is `FROM scratch` (a static x86_64 musl binary and CA certificates). `lrclib-mirror --version` names the
web search's upstream commit it was built from.

## Usage

```yaml
services:
  lrclib:
    image: ghcr.io/nikarh/lrclib-mirror:v2
    restart: unless-stopped
    read_only: true
    cap_drop: [ALL]
    security_opt: [no-new-privileges:true]
    user: "1000:1000"
    volumes:
      - /srv/lrclib:/data
    ports:
      - "3300:3300"
```

- `read_only: true`: nothing is written outside `/data`, no tmpfs needed.
- `cap_drop: [ALL]`, `no-new-privileges`: it needs no privileges; it listens on 3300.
- `user`: any uid:gid that can write `/data` (the image defaults to nobody, 65534). A new named volume is writable by
  any user; a bind mount must belong to the chosen one.

The first start downloads and imports the newest dump (see [Importing](#importing)); until then the API answers
`503` (an interrupted import continues at the next start). Then point clients at it instead of `https://lrclib.net`, e.g.
`http://lrclib:3300/api/get?artist_name=...`.

The data directory holds `stores/<dump>/` (the store being served; `<dump>.partial/` while one is imported) and,
with updates off, `dumps/`.

## Configuration

Flags, each with an environment variable fallback:

| Flag | Environment | Default | |
| --- | --- | --- | --- |
| `--data-dir` | `LRCLIB_MIRROR_DATA_DIR` | `/data` | Stores (`stores/`) and dumps to import (`dumps/`) |
| `--listen` | `LRCLIB_MIRROR_LISTEN` | `0.0.0.0:3300` | HTTP address |
| `--update-schedule` | `LRCLIB_MIRROR_UPDATE_SCHEDULE` | `34 2 * * *` | When to look for a newer dump, besides at start: a cron expression (`minute hour day month weekday`, e.g. `0 */6 * * *`, `30 4 * * mon`, or `@daily`) in the time zone of `TZ` (see below); `off`: never. The default is every day at 02:34; after a failed check the next one comes within an hour |
| `--min-free-gb` | `LRCLIB_MIRROR_MIN_FREE_GB` | `60` | Free space an import needs to start (what an interrupted import of the same dump has counts as free) |
| `--import-threads` | `LRCLIB_MIRROR_IMPORT_THREADS` | the CPUs (of the container's quota) | Threads importing a dump |
| `--import-memory` | `LRCLIB_MIRROR_IMPORT_MEMORY` | 60% of the container's limit or the machine's memory, 768 to 4096 | MB an import may take, roughly (at least 768) |
| `--listing-url` | `LRCLIB_MIRROR_LISTING_URL` | LRCLIB's | The dump listing |
| `--dump-url` | `LRCLIB_MIRROR_DUMP_URL` | LRCLIB's | Where dumps are downloaded from |

`TZ` sets the time zone of the schedule, as an IANA name (`TZ=Europe/Riga`); without it, the system's time zone
(UTC in the container image). The time zone database is built in, so the `FROM scratch` image needs no files for it.
`--update-interval` was replaced by `--update-schedule`: a start with `--update-interval` or
`LRCLIB_MIRROR_UPDATE_INTERVAL` set fails, rather than ignore it.

`RUST_LOG` sets the log filter (default `info,tantivy=warn`).

With updates off, put a dump into `<data-dir>/dumps/`, named like LRCLIB names it
(`lrclib-db-dump-<date>.sqlite3.gz`, or unpacked as `.sqlite3`): it is imported at start when it is newer than the
store. A store can also be built by hand, e.g. on another machine:
`lrclib-mirror import lrclib-db-dump-<date>.sqlite3.gz <data-dir>/stores/lrclib-db-dump-<date>` (run again into
the same directory, an interrupted `import` continues).

## Same answers as LRCLIB

`tools/compare` runs the same requests against LRCLIB's server and lrclib-mirror, both on the same dump, and checks
that the answers are the same, byte for byte (searches: the same results with the same scores; tracks that score
the same may come in another order, as they do between LRCLIB's own index segments). It makes the requests from
tracks of the store: lookups by id, by name and artist (with and without album and duration, with other spellings),
searches by words and by fields, and requests that must fail. Then it times both.

```sh
lrclib serve -d lrclib-db-dump-<date>.sqlite3 -p 3397        # LRCLIB's server (github.com/tranxuanthang/lrclib)
lrclib-mirror --data-dir data --update-schedule off           # with the same dump in data/dumps/
mise run compare -- --old http://127.0.0.1:3397 --new http://127.0.0.1:3300 --store data/stores/lrclib-db-dump-<date>
```

The last run, on the 2026-10-30 dump (34.6 M tracks): of 24 825 answers (from 4000 tracks, and the edge cases),
none differ. Before that, LRCLIB's own code checked the name normalization, the plain lyrics derived from synced
ones and every Lyricsfile built, on 1.5 M tracks of the dump: no difference.

The tests import a database with LRCLIB's schema and rows of every kind through the streaming importer and check
every track, and the track every metadata lookup finds, against SQLite answering LRCLIB's server's queries.

## Verify downloads

```sh
curl -LO https://github.com/nikarh/lrclib-mirror/releases/download/v2/lrclib-mirror-x86_64-unknown-linux-musl
curl -LO https://github.com/nikarh/lrclib-mirror/releases/download/v2/SHA256SUMS
curl -LO https://github.com/nikarh/lrclib-mirror/releases/download/v2/SHA256SUMS.asc
curl -L https://raw.githubusercontent.com/nikarh/lrclib-mirror/main/docs/signing-key.asc | gpg --import
gpg --verify SHA256SUMS.asc SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
```

The key is `46D28A16740135310E5676BCCEA1A710FEC72835` (ed25519, expires 2029-11-06).

## Development

[mise](https://mise.jdx.dev) installs the tools; `mise run check` runs everything CI does, `mise run build` builds the
binary with the web search, `mise run docker` the image, `mise run compare` compares with LRCLIB's server.
`mise run patch` applies `patches/` to the web search submodule (clone with `--recursive`, or
`git submodule update --init`).

## License

This project's own code is MIT or Apache-2.0, at your option. Parts of it are ported from LRCLIB's server
([tranxuanthang/lrclib](https://github.com/tranxuanthang/lrclib), MIT): the name normalization, the Lyricsfile and
LRC handling, the search queries and the API's behavior; its license is `licenses/LICENSE-lrclib`. The web search
is MIT licensed by its author, and so is the change to it in `patches/`. Both license texts come with the release
files (`LICENSE-lrclib*`). The LRCLIB database is LRCLIB's.
