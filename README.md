# lrclib-mirror

A self-hosted, read-only [LRCLIB](https://lrclib.net): the LRCLIB API and its web search, answered from a local copy
of LRCLIB's [public database dump](https://lrclib.net/db-dumps), which it keeps up to date by itself. Useful when
a music library needs lyrics for many tracks: no rate limits, no network round trips.

This project mostly packs other people's work into one binary and image:

- [**tranxuanthang/lrclib**](https://github.com/tranxuanthang/lrclib): the LRCLIB server, used as a library. Its
  lookups (`GET /api/get`, `/api/get/{id}`, `/api/search`) answer exactly as on lrclib.net. Publishing and flagging
  lyrics are left out: the next dump replaces the copy.
- [**tranxuanthang/lrclib-homepage**](https://github.com/tranxuanthang/lrclib-homepage): the lrclib.net web page,
  reduced to the search and the lyrics view, asking the local API.
- LRCLIB's database dumps, downloaded at start and whenever a newer one is out (daily check). A new dump is switched
  to without a restart, once its search index has caught up; until then the old one keeps answering.

Both upstream projects are git submodules (`upstream/`), changed only by the small patches in `patches/`. A daily
workflow moves them to the newest upstream commits in a pull request.

## Images and downloads

There are no versions, only builds of the newest upstream code:

| | Image | Release (binary, signed checksums) |
| --- | --- | --- |
| `legacy-sqlite` | `ghcr.io/nikarh/lrclib-mirror:v1` (older builds: `:sha-<commit>`) | [v1 Latest (`v1`)](https://github.com/nikarh/lrclib-mirror/releases/tag/v1) |
| pending upstream update | `ghcr.io/nikarh/lrclib-mirror:v1-dev` | [v1 Dev (`v1-dev`)](https://github.com/nikarh/lrclib-mirror/releases/tag/v1-dev) |

The image is `FROM scratch` (a static x86_64 musl binary and CA certificates). `lrclib-mirror --version` names the
upstream commits it was built from.

## Disk space

The dump is ~50 GB to download and ~270 GB unpacked (2026). It is unpacked while it downloads, so the compressed
file is never stored, but during an update the old and the new database both exist. On a filesystem with
transparent compression (btrfs `compress=zstd`) it takes about a quarter of that. The search index adds ~2.5 GB.
Reads are random, so an SSD answers fastest; a hard disk works too (a lookup is a few seeks).

## Usage

```yaml
services:
  lrclib:
    image: ghcr.io/nikarh/lrclib-mirror:v1
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

The first start downloads the newest dump and builds the search index, which takes a while (hours on a slow
machine); the API answers `503` until the database is there, and searches fail until the index has caught up.

Then point clients at it instead of `https://lrclib.net`, e.g. `http://lrclib:3300/api/get?artist_name=...`.

## Configuration

Flags, each with an environment variable fallback:

| Flag | Environment | Default | |
| --- | --- | --- | --- |
| `--data-dir` | `LRCLIB_MIRROR_DATA_DIR` | `/data` | Dumps (`dumps/`) and the search index (`search-index/`) |
| `--listen` | `LRCLIB_MIRROR_LISTEN` | `0.0.0.0:3300` | HTTP address |
| `--update-interval` | `LRCLIB_MIRROR_UPDATE_INTERVAL` | `24h` | How often to look for a newer dump (`30m`, `7d`); `0`: never |
| `--min-free-gb` | `LRCLIB_MIRROR_MIN_FREE_GB` | `300` | Free space a download needs before it starts |
| `--listing-url` | `LRCLIB_MIRROR_LISTING_URL` | LRCLIB's | The dump listing |
| `--dump-url` | `LRCLIB_MIRROR_DUMP_URL` | LRCLIB's | Where dumps are downloaded from |

`RUST_LOG` sets the log filter (default `info,tantivy=warn`). The upstream server's own variables
(`LRCLIB_MMAP_SIZE`, `LRCLIB_CACHE_SIZE`, `LRCLIB_POOL_MAX_SIZE`, `LRCLIB_MAX_CONCURRENT_LOADS`, ...) apply too.

With updates off, put a dump into `<data-dir>/dumps/` yourself, unpacked and named like LRCLIB names it
(`lrclib-db-dump-<date>.sqlite3`).

## Verify downloads

```sh
curl -LO https://github.com/nikarh/lrclib-mirror/releases/download/v1/lrclib-mirror-x86_64-unknown-linux-musl
curl -LO https://github.com/nikarh/lrclib-mirror/releases/download/v1/SHA256SUMS
curl -LO https://github.com/nikarh/lrclib-mirror/releases/download/v1/SHA256SUMS.asc
curl -L https://raw.githubusercontent.com/nikarh/lrclib-mirror/main/docs/signing-key.asc | gpg --import
gpg --verify SHA256SUMS.asc SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
```

The key is `46D28A16740135310E5676BCCEA1A710FEC72835` (ed25519, expires 2029-11-06).

## Development

[mise](https://mise.jdx.dev) installs the tools; `mise run check` runs everything CI does, `mise run build` builds the
binary with the web search, `mise run docker` the image. `mise run patch` applies `patches/` to the submodules
(clone with `--recursive`, or `git submodule update --init`).

## License

This project's own code is MIT or Apache-2.0, at your option. The upstream projects are MIT licensed by their
author, and so are the changes to them in `patches/`; their license texts come with the release files
(`LICENSE-lrclib*`). The LRCLIB database is LRCLIB's.
